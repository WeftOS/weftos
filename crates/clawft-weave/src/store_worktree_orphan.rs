//! `store.worktree.orphan` and the orphan half of `store.worktree.gc` (WO-069 lane E, prototype).
//! A child of `store_worktree` so it shares that module's private helpers. The policy and the
//! actions are in `weftos_store::orphan`; this only wires them to the daemon: the clock comes in
//! as `now_ms`, entries are signed when a key is set, a retired store's bucket is dropped through
//! the store service, and marking and retiring are recorded in the activity log and the chain.

use weftos_store::orphan::{OrphanError, OrphanPolicy, Refusal, mark_orphan_with, plan_orphans, plan_orphans_from, retire_orphan};
use weftos_store::overlay::S2Error;

use super::*;

fn policy() -> Result<OrphanPolicy, String> {
    OrphanPolicy::from_env().map_err(|e| e.to_string())
}

fn policy_json(p: &OrphanPolicy) -> Value {
    json!({"orphan_after_ms": p.orphan_after_ms, "archive_after_ms": p.archive_after_ms})
}

/// `orphan <slug> --since <ms>`: the sweep reports when the worktree's path first went missing; the
/// daemon decides. A path missing for less than the grace is not an error (`marked: false`), so the
/// sweep can call this on every pass; every other refusal is.
pub(super) fn mark(svc: &StoreService, b: &Op, n: &Names, slug: &str, since_ms: u64, now_ms: u64, sign: &SignCfg, by: Option<&str>) -> Result<Value, String> {
    let pol = policy()?;
    let write = |ent: &RegistryEntry| match &sign.key {
        Some(k) => write_registry_entry_signed(b, &n.reg, ent, &Ed(k.clone())),
        None => write_registry_entry(b, &n.reg, ent),
    };
    match mark_orphan_with(b, &n.reg, slug, now_ms, since_ms, &pol, &write) {
        Ok(e) => {
            let o = e.orphan.as_ref().ok_or("orphan entry without its record")?;
            let fresh = o.orphaned_at_ms == now_ms;
            if fresh {
                let actor = by.unwrap_or("sweep@daemon");
                let detail = json!({"store": n.wt_name.to_string(), "slug": slug, "last_head": o.last_head, "missing_since_ms": o.missing_since_ms});
                svc.record_act(actor, &n.wt_name, "WorktreeOrphan", None, 0, weftos_store::Outcome::Ok, Some(("store.worktree.orphan", detail)));
            }
            Ok(json!({"slug": slug, "marked": true, "new": fresh, "state": "orphan", "orphaned_at_ms": o.orphaned_at_ms,
                      "missing_since_ms": o.missing_since_ms, "last_head": o.last_head, "policy": policy_json(&pol)}))
        }
        Err(OrphanError::Refused(Refusal::TooSoon { missing_ms, grace_ms })) => {
            Ok(json!({"slug": slug, "marked": false, "reason": "grace", "missing_ms": missing_ms, "grace_ms": grace_ms}))
        }
        Err(x) => Err(x.to_string()),
    }
}

/// The orphan part of `gc`: always a plan (every orphan with its age, size and whether it is due);
/// with `confirmed`, each due one is archived and deleted. Returns the keys to merge into gc's answer.
#[allow(clippy::too_many_arguments)]
/// `plan_cache`: a display plan (a dry run) may list the registry through the daemon's cache.
pub(super) fn gc(
    svc: &StoreService,
    b: &Op,
    mesh: &str,
    project: &str,
    reg: &str,
    now_ms: u64,
    confirmed: bool,
    registry_store: bool,
    plan_cache: Option<std::sync::Arc<weftos_store::follower_cache_shared::SharedCache>>,
    actor: Option<&str>,
) -> Result<Value, String> {
    let pol = policy()?;
    let plan = match plan_cache.filter(|_| !confirmed) {
        Some(c) => c.registry(b, reg).and_then(|l| plan_orphans_from(b, l.into_entries(), now_ms, &pol)),
        None => plan_orphans(b, reg, now_ms, &pol),
    }
    .map_err(|x| x.to_string())?;
    let (mut retired, mut failed) = (Vec::new(), Vec::new());
    if confirmed {
        for c in plan.iter().filter(|c| c.due) {
            let r = names_with(mesh, project, &c.slug, registry_store).and_then(|nm| {
                // Only this slug's own worktree bucket: never a bucket an entry merely names.
                if nm.wt != c.bucket {
                    return Err(format!("entry {} names bucket {}, not its worktree bucket {}", c.slug, c.bucket, nm.wt));
                }
                svc.ensure_archive().map_err(|x| x.to_string())?;
                let prefix = archive_key(&nm.wt, now_ms, "");
                let drop_bucket = |_: &str| match svc.archive(&nm.wt_name, true, actor.unwrap_or("gc")) {
                    Ok(_) => Ok(()),
                    Err(x) if x.to_string().contains("no such bucket") => Ok(()),
                    Err(x) => Err(S2Error::Backend(x.to_string())),
                };
                let done = retire_orphan(b, reg, &c.slug, now_ms, &pol, &ArchiveTarget { bucket: ARCHIVE_BUCKET, prefix: &prefix }, &drop_bucket).map_err(|x| x.to_string())?;
                let by = actor.map_or_else(|| "gc@daemon".to_owned(), str::to_owned);
                let detail = json!({"store": nm.wt_name.to_string(), "slug": c.slug, "archived": done.archived, "bytes": done.bytes, "archive_prefix": prefix, "actor": by});
                svc.record_act(&by, &nm.wt_name, "WorktreeOrphanRetire", None, done.bytes, weftos_store::Outcome::Ok, Some(("store.worktree.orphan_retire", detail)));
                Ok(c.slug.clone())
            });
            match r {
                Ok(s) => retired.push(s),
                Err(x) => failed.push(json!({"slug": c.slug, "error": x})),
            }
        }
    }
    Ok(json!({"orphans": plan, "orphans_retired": retired, "orphans_failed": failed, "orphan_policy": policy_json(&pol)}))
}

/// `base` (gc's own answer) with the orphan keys added.
pub(super) fn merged(mut base: Value, extra: Value) -> Value {
    if let (Some(m), Value::Object(x)) = (base.as_object_mut(), extra) {
        m.extend(x);
    }
    base
}

#[cfg(test)]
#[path = "store_worktree_orphan_tests.rs"]
mod tests;
