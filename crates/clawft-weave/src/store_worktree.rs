//! `store.worktree.*`: the daemon side of ADR-126 S2 (worktree stores, tree sharing, the L1
//! follower view). `scripts/worktree-store.sh` calls these through `weaver store worktree <verb>`.
//!
//! Every verb is an Admin act on the local daemon (see `capability.rs`). They work on the raw data
//! plane (`StoreService::operator_objects`), the same level as S1's promote and archive, and the
//! ones that change shared state record a chain event. Paths in params (`root`, `dest`, `cache`,
//! the `*_z` lists) are read or written by the daemon itself, so client and daemon share a host.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use weftos_store::backend::{BackendError, ObjectBackend as S1, ObjectMeta};
use weftos_store::follower::{self, Change, FollowState, LeftoverPolicy, Why};
use weftos_store::promote::{ArchiveTarget, PromotionPolicy, WorktreeMeta, promote, write_meta};
use weftos_store::service::{ARCHIVE_BUCKET, Parties, archive_key};
use weftos_store::share::{
    Actor, Limits, TreeSource, claim_merge, mark_handoff, mark_merged, parse_name_status_z, publish_tree,
};
use weftos_store::share_fetch::{
    RegistryEntry, Select, fetch_bundle, fetch_tree, list_shared, summarize, write_registry_entry,
};
use weftos_store::{StoreName, StoreService};

/// Delegates to the service's shared backend so the S2 code (generic over a sized backend) can use it.
struct Op(Arc<dyn S1>);

impl S1 for Op {
    fn put(&self, b: &str, k: &str, d: &[u8], n: bool) -> Result<ObjectMeta, BackendError> {
        self.0.put(b, k, d, n)
    }
    fn get(&self, b: &str, k: &str) -> Result<Vec<u8>, BackendError> {
        self.0.get(b, k)
    }
    fn head(&self, b: &str, k: &str) -> Result<ObjectMeta, BackendError> {
        self.0.head(b, k)
    }
    fn delete(&self, b: &str, k: &str) -> Result<(), BackendError> {
        self.0.delete(b, k)
    }
    fn copy(&self, sb: &str, sk: &str, db: &str, dk: &str) -> Result<ObjectMeta, BackendError> {
        self.0.copy(sb, sk, db, dk)
    }
    fn list(&self, b: &str, p: &str) -> Result<Vec<ObjectMeta>, BackendError> {
        self.0.list(b, p)
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct P {
    project: Option<String>,
    slug: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    head: Option<String>,
    root: Option<String>,
    dest: Option<String>,
    cache: Option<String>,
    state: Option<String>,
    query: Option<String>,
    a: Option<String>,
    b: Option<String>,
    #[serde(rename = "paths_z")]
    paths_z: Option<String>,
    uncommitted_z: Option<String>,
    changes_z: Option<String>,
    bundle: Option<String>,
    bundle_out: Option<String>,
    handoff: Option<String>,
    #[serde(default)]
    force: bool,
    /// Output format; the CLI formats, the daemon ignores it.
    #[serde(default)]
    #[allow(dead_code)]
    json: bool,
    #[serde(default)]
    confirmed: bool,
    /// Who is acting; filled in by the CLI from the environment.
    agent: Option<String>,
    host: Option<String>,
}

fn need<'a>(v: &'a Option<String>, what: &str) -> Result<&'a str, String> {
    v.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| format!("missing --{what}"))
}

fn slug_ok(s: &str) -> Result<&str, String> {
    let ok = !s.is_empty()
        && s.len() <= 48
        && s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !s.starts_with('-');
    if ok { Ok(s) } else { Err(format!("bad slug {s:?} (want [a-z0-9-], max 48)")) }
}

fn project_ok(s: &str) -> Result<&str, String> {
    if !s.is_empty() && s.len() <= 40 && s.bytes().all(|c| c.is_ascii_alphanumeric()) {
        Ok(s)
    } else {
        Err(format!("bad project id {s:?}"))
    }
}

fn read_list(path: &Option<String>) -> Result<Vec<u8>, String> {
    match path {
        None => Ok(Vec::new()),
        Some(p) => {
            let md = std::fs::metadata(p).map_err(|e| format!("{p}: {e}"))?;
            if !md.is_file() || md.len() > 32 << 20 {
                return Err(format!("{p}: not a regular file under 32 MiB"));
            }
            std::fs::read(p).map_err(|e| format!("{p}: {e}"))
        }
    }
}

fn nul_list(raw: &[u8]) -> Result<Vec<String>, String> {
    raw.split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .map(|t| std::str::from_utf8(t).map(str::to_owned).map_err(|_| "non-UTF-8 path in list".to_string()))
        .collect()
}

fn who(p: &P) -> Actor {
    Actor {
        agent: p.agent.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "agent".into()),
        host: p.host.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "unknown".into()),
    }
}

struct Names {
    wt_name: StoreName,
    wt: String,
    pr_name: StoreName,
    pr: String,
}

fn names(mesh: &str, project: &str, slug: &str) -> Result<Names, String> {
    let e = |e: weftos_store::StoreError| e.to_string();
    let wt_name = StoreName::worktree(mesh, project_ok(project)?, slug_ok(slug)?).map_err(e)?;
    let pr_name = StoreName::project(mesh, project).map_err(e)?;
    Ok(Names { wt: wt_name.bucket(), pr: pr_name.bucket(), wt_name, pr_name })
}

fn entry_json(e: &RegistryEntry) -> Value {
    json!({
        "slug": e.slug, "branch": e.branch, "head": e.head_commit, "state": e.state,
        "writer": format!("{}@{}", e.writer.agent, e.writer.host),
        "merger": e.merger.as_ref().map(|m| format!("{}@{}", m.agent, m.host)),
        "docs": e.doc_count, "files": e.file_count, "updated_ms": e.updated_ms,
    })
}

fn change_json(c: &Change) -> Value {
    match c {
        Change::New(e) => json!({"change": "new", "worktree": entry_json(e)}),
        Change::HeadMoved { entry, from } => json!({"change": "head_moved", "from": from, "worktree": entry_json(entry)}),
        Change::StateChanged { entry, from } => json!({"change": "state_changed", "from": from, "worktree": entry_json(entry)}),
        Change::Gone { slug } => json!({"change": "gone", "slug": slug}),
        Change::Unreadable { slug } => json!({"change": "unreadable", "slug": slug}),
    }
}

fn why_str(w: Why) -> &'static str {
    match w {
        Why::ForgottenOpen => "forgotten_open",
        Why::UnclaimedHandoff => "unclaimed_handoff",
        Why::AbandonedClaim => "abandoned_claim",
        Why::StuckCleanup => "stuck_cleanup",
        Why::FutureTimestamp => "future_timestamp",
    }
}

/// Re-derive one worktree's registry entry from its own bucket (the daemon is the only writer).
fn refresh(b: &Op, pr: &str, slug: &str, wt: &str) -> Result<(), String> {
    let e = |e: weftos_store::overlay::S2Error| e.to_string();
    if let Some(ent) = summarize(b, slug, wt).map_err(e)? {
        write_registry_entry(b, pr, &ent).map_err(e)?;
    }
    Ok(())
}

/// Run one verb. `mesh` is the store-name mesh token of this daemon.
pub fn run(svc: &StoreService, mesh: &str, method: &str, params: Value, now_ms: u64) -> Result<Value, String> {
    let p: P = serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;
    let e = |e: weftos_store::overlay::S2Error| e.to_string();
    let b = Op(svc.operator_objects());
    let project = project_ok(need(&p.project, "project")?)?.to_owned();
    let verb = method.strip_prefix("store.worktree.").unwrap_or(method);
    match verb {
        "buckets" => {
            let n = names(mesh, &project, need(&p.slug, "slug")?)?;
            Ok(json!({"worktree": n.wt, "project": n.pr}))
        }
        "create" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            // The project store may not exist yet on a fresh node; create it, never fail on "exists".
            svc.ensure_store(&n.pr_name, "cli", Parties::defaults_for(&n.pr_name)).map_err(|e| e.to_string())?;
            svc.ensure_store(&n.wt_name, "cli", Parties::defaults_for(&n.wt_name)).map_err(|e| e.to_string())?;
            if weftos_store::promote::is_promoted(&b, &n.wt).map_err(e)?.is_some() {
                return Err(format!("worktree {slug} was already landed; use a new slug"));
            }
            write_meta(
                &b,
                &n.wt,
                &WorktreeMeta {
                    slug: slug.into(),
                    project: project.clone(),
                    branch: need(&p.branch, "branch")?.into(),
                    base_commit: need(&p.base, "base")?.into(),
                    created_ms: now_ms,
                },
            )
            .map_err(e)?;
            Ok(json!({"worktree": n.wt, "project": n.pr}))
        }
        "publish" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            let root = PathBuf::from(need(&p.root, "root")?);
            if !root.is_absolute() || !root.is_dir() {
                return Err("--root must be an absolute directory".into());
            }
            let paths = nul_list(&read_list(&p.paths_z)?)?;
            let uncommitted = nul_list(&read_list(&p.uncommitted_z)?)?;
            let changes = parse_name_status_z(&read_list(&p.changes_z)?).map_err(e)?;
            let bundle = p.bundle.as_deref().map(Path::new);
            let writer = who(&p);
            let rep = publish_tree(
                &b,
                &n.wt,
                &TreeSource {
                    root: &root,
                    paths: &paths,
                    uncommitted: &uncommitted,
                    changes: &changes,
                    branch: need(&p.branch, "branch")?,
                    base_commit: need(&p.base, "base")?,
                    head_commit: need(&p.head, "head")?,
                    bundle,
                    writer: writer.clone(),
                },
                &Limits::default(),
                now_ms,
            )
            .map_err(e)?;
            if let Some(note) = p.handoff.as_deref() {
                mark_handoff(&b, &n.wt, &writer, note, now_ms).map_err(e)?;
            }
            refresh(&b, &n.pr, slug, &n.wt)?;
            Ok(json!({
                "uploaded": rep.uploaded, "unchanged": rep.unchanged, "removed": rep.removed,
                "skipped": rep.skipped.iter().map(|(k, w)| format!("{k}: {w:?}")).collect::<Vec<_>>(),
                "bundle_uploaded": rep.bundle_uploaded, "handoff": p.handoff.is_some(),
            }))
        }
        "fetch" => {
            let n = names(mesh, &project, need(&p.slug, "slug")?)?;
            let dest = PathBuf::from(need(&p.dest, "dest")?);
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            let r = fetch_tree(&b, &n.wt, &dest, Select::All, p.force).map_err(e)?;
            let mut head = None;
            if let Some(out) = &p.bundle_out {
                head = Some(fetch_bundle(&b, &n.wt, Path::new(out)).map_err(e)?);
            }
            Ok(json!({
                "written": r.written.len(), "unchanged": r.unchanged.len(), "conflicts": r.conflicts,
                "missing": r.missing, "refused": r.refused, "bundle_head": head,
            }))
        }
        "claim" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            let h = claim_merge(&b, &n.wt, &who(&p), now_ms).map_err(e)?;
            refresh(&b, &n.pr, slug, &n.wt)?;
            Ok(json!({"state": h.state, "merger": h.merger.map(|m| format!("{}@{}", m.agent, m.host))}))
        }
        "promote" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            svc.ensure_archive().map_err(|e| e.to_string())?;
            let prefix = archive_key(&n.wt, now_ms, "");
            let r = promote(
                &b,
                &n.wt,
                &n.pr,
                &ArchiveTarget { bucket: ARCHIVE_BUCKET, prefix: &prefix },
                &PromotionPolicy::default(),
                now_ms,
            )
            .map_err(e)?;
            // A land that went through git is the merge: record it if a claimer did not. Only after
            // the promotion succeeded, so a conflict leaves the handoff claimable and republishable.
            if let Ok(Some(h)) = weftos_store::share::read_handoff(&b, &n.wt)
                && h.state == weftos_store::share::State::Claimed
            {
                mark_merged(&b, &n.wt, &who(&p), "landed", now_ms).map_err(e)?;
            }
            Ok(json!({
                "promoted": r.promoted, "deleted": r.deleted, "kept_project": r.kept_project,
                "archived": r.archived.len(), "dropped_tombstones": r.dropped_tombstones,
            }))
        }
        "follow" => {
            let n = names(mesh, &project, "x")?;
            let sp = PathBuf::from(need(&p.state, "state")?);
            let mut st = FollowState::load(&sp).map_err(e)?;
            let ch = if p.confirmed { follower::poll_confirmed(&b, &n.pr, &mut st) } else { follower::poll(&b, &n.pr, &mut st) }
                .map_err(e)?;
            st.save(&sp).map_err(e)?;
            Ok(json!({"changes": ch.iter().map(change_json).collect::<Vec<_>>()}))
        }
        "search" => {
            let n = names(mesh, &project, "x")?;
            let entries = list_shared(&b, &n.pr).map_err(e)?;
            let r = follower::search_docs(&b, &entries, need(&p.query, "query")?);
            Ok(json!({
                "hits": r.hits.iter().map(|h| json!({"slug": h.slug, "path": h.path, "title": h.title, "state": h.state})).collect::<Vec<_>>(),
                "unreadable": r.unreadable,
            }))
        }
        "leftovers" => {
            let n = names(mesh, &project, "x")?;
            let entries = weftos_store::share_fetch::list_shared_lenient(&b, &n.pr).map_err(e)?.0;
            let l = follower::leftovers(&entries, now_ms, &LeftoverPolicy::default());
            Ok(json!({"leftovers": l.iter().map(|x| json!({"why": why_str(x.why), "idle_ms": x.idle_ms, "worktree": entry_json(&x.entry)})).collect::<Vec<_>>()}))
        }
        "sync-docs" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            let entry = summarize(&b, slug, &n.wt).map_err(e)?.ok_or("worktree has nothing published")?;
            let r = follower::sync_docs(&b, &entry, Path::new(need(&p.cache, "cache")?)).map_err(e)?;
            Ok(json!({"written": r.written.len(), "unchanged": r.unchanged.len(), "refused": r.refused}))
        }
        "overlaps" => {
            let (sa, sb) = (need(&p.a, "a")?, need(&p.b, "b")?);
            let (na, nb) = (names(mesh, &project, sa)?, names(mesh, &project, sb)?);
            let ea = summarize(&b, sa, &na.wt).map_err(e)?.ok_or_else(|| format!("{sa} has nothing published"))?;
            let eb = summarize(&b, sb, &nb.wt).map_err(e)?.ok_or_else(|| format!("{sb} has nothing published"))?;
            let r = follower::overlaps(&b, &ea, &eb).map_err(e)?;
            Ok(json!({
                "same_base": r.same_base,
                "overlaps": r.overlaps.iter().map(|o| json!({"path": o.path, "kind": format!("{:?}", o.kind)})).collect::<Vec<_>>(),
            }))
        }
        other => Err(format!("unknown worktree verb: {other}")),
    }
}

/// Dispatch one `store.worktree.*` verb on the daemon's service.
pub async fn handle(method: &str, params: Value) -> clawft_rpc::Response {
    let Some(svc) = crate::store_rt::global() else {
        return clawft_rpc::Response::error("the object store is not running on this daemon".to_string());
    };
    let method = method.to_owned();
    let r = tokio::task::spawn_blocking(move || {
        run(&svc, &crate::store_rt::mesh_token(), &method, params, clawft_kernel::exposure::registry::now_ms())
    })
    .await;
    match r {
        Ok(Ok(v)) => clawft_rpc::Response::success(v),
        Ok(Err(e)) => clawft_rpc::Response::error(e),
        Err(e) => clawft_rpc::Response::error(format!("store task failed: {e}")),
    }
}

#[cfg(test)]
#[path = "store_worktree_tests.rs"]
mod tests;
