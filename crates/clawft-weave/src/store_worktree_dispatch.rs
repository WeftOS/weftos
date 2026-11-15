//! The verb dispatcher of `store.worktree.*`: one `match` over the verb, each arm a verb. A child of
//! `store_worktree` so it shares that module's private types and helpers.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn dispatch(svc: &StoreService, mesh: &str, method: &str, params: Value, now_ms: u64, registry_store: bool, sign: &SignCfg, guard: &Guard) -> Result<Value, String> {
    let names = |m: &str, pr: &str, s: &str| names_with(m, pr, s, registry_store);
    let p: P = serde_json::from_value(params).map_err(|e| format!("invalid params: {e}"))?;
    let e = |e: weftos_store::overlay::S2Error| e.to_string();
    let b = Op(svc.operator_objects());
    let project = project_ok(need(&p.project, "project")?)?.to_owned();
    let verb = method.strip_prefix("store.worktree.").unwrap_or(method);
    if registry_store && !matches!(verb, "buckets") {
        // No admins, writers or appenders: only the daemon's operator path can write it. Idempotent,
        // so a worktree created before the switch was turned on still finds its registry store.
        let rn = StoreName::release(mesh, &project, REGISTRY_CHANNEL).map_err(|e| e.to_string())?;
        svc.ensure_store(&rn, "cli", Parties::default()).map_err(|e| e.to_string())?;
    }
    match verb {
        "buckets" => {
            let n = names(mesh, &project, need(&p.slug, "slug")?)?;
            Ok(json!({"worktree": n.wt, "project": n.pr}))
        }
        "create" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            // The project store may not exist yet on a fresh node; create it, never fail on "exists".
            // "Exists" is fine even when this daemon's own state does not know the store: another
            // daemon on the same backend made it (the project store is shared; its record and
            // grants live with the daemon that created it). The S2 verbs use the raw data plane.
            for name in [&n.pr_name, &n.wt_name] {
                match svc.ensure_store(name, "cli", Parties::defaults_for(name)) {
                    Ok(_) | Err(weftos_store::StoreError::Exists(_)) => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
            // Re-creating a live worktree is idempotent and keeps its first creation time. A slug
            // whose worktree already landed (or was archived) must not be reused until that store
            // is archived and deleted: its handoff and promotion record would carry over.
            let mut created_ms = now_ms;
            if let Some(m) = read_meta(&b, &n.wt).map_err(e)? {
                let landed = is_promoted(&b, &n.wt).map_err(e)?.is_some()
                    || weftos_store::share::read_handoff(&b, &n.wt)
                        .map_err(e)?
                        .is_some_and(|h| matches!(h.state, weftos_store::share::State::Merged | weftos_store::share::State::Archived))
                    || svc.is_archived(&n.wt_name) == Some(true);
                if landed {
                    return Err(format!("worktree {slug} already landed or was archived; run `store archive --delete` on it before reusing the slug"));
                }
                created_ms = m.created_ms;
            }
            write_meta(
                &b,
                &n.wt,
                &WorktreeMeta {
                    slug: slug.into(),
                    project: project.clone(),
                    branch: need(&p.branch, "branch")?.into(),
                    base_commit: need(&p.base, "base")?.into(),
                    created_ms,
                },
            )
            .map_err(e)?;
            Ok(json!({"worktree": n.wt, "project": n.pr}))
        }
        "publish" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            live_worktree(svc, &b, &n, &project, slug)?;
            let quota = match p.quota_bytes {
                Some(0) => None,
                Some(q) => Some(q),
                None => Some(weftos_store::retention::DEFAULT_WORKTREE_QUOTA),
            };
            if let Some(q) = quota {
                let (_, used) = weftos_store::retention::usage(&b, &n.wt).map_err(e)?;
                if used > q {
                    return Err(format!(
                        "worktree store {slug} holds {used} bytes, over its {q} byte quota; \
                         land or clean it, or raise --quota-bytes"
                    ));
                }
            }
            let root = guard.check(need(&p.root, "root")?, "root", Use::ReadDir)?;
            // The hook publishes from the top of a git worktree; anything else is not a worktree.
            if !root.join(".git").exists() {
                return Err("--root must be the top of a git worktree (no .git there)".into());
            }
            let paths = nul_list(&read_list(guard, &p.paths_z, "paths-z")?)?;
            let uncommitted = nul_list(&read_list(guard, &p.uncommitted_z, "uncommitted-z")?)?;
            let changes = parse_name_status_z(&read_list(guard, &p.changes_z, "changes-z")?).map_err(e)?;
            // A listed path that resolves into the daemon's own state is refused outright, not skipped.
            if let Some(bad) = paths.iter().find(|rel| guard.is_protected(&root.join(rel))) {
                return Err(format!("{bad}: inside the daemon's own state; refused"));
            }
            let bundle = match p.bundle.as_deref() {
                Some(b) => Some(guard.check(b, "bundle", Use::ReadFile)?),
                None => None,
            };
            let bundle = bundle.as_deref();
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
            refresh(&b, &n.reg, slug, &n.wt, sign)?;
            Ok(json!({
                "uploaded": rep.uploaded, "unchanged": rep.unchanged, "removed": rep.removed,
                "skipped": rep.skipped.iter().map(|(k, w)| format!("{k}: {w:?}")).collect::<Vec<_>>(),
                "bundle_uploaded": rep.bundle_uploaded, "handoff": p.handoff.is_some(),
            }))
        }
        "fetch" => {
            let n = names(mesh, &project, need(&p.slug, "slug")?)?;
            let dest = guard.check(need(&p.dest, "dest")?, "dest", Use::WriteDir)?;
            let out = match &p.bundle_out {
                Some(o) => Some(guard.check(o, "bundle-out", Use::WriteFile)?),
                None => None,
            };
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            let r = fetch_tree(&b, &n.wt, &dest, Select::All, p.force).map_err(e)?;
            let mut head = None;
            let mut note = None;
            if let Some(out) = &out {
                // A tree with no commits past its base carries no bundle: the tree is still fetched.
                if weftos_store::share::read_manifest(&b, &n.wt).map_err(e)?.is_some_and(|m| m.has_bundle) {
                    head = Some(fetch_bundle(&b, &n.wt, out).map_err(e)?);
                } else {
                    note = Some("this tree was published without a git bundle; no commits to fetch");
                }
            }
            Ok(json!({
                "written": r.written.len(), "unchanged": r.unchanged.len(), "conflicts": r.conflicts,
                "missing": r.missing, "refused": r.refused, "bundle_head": head, "note": note,
            }))
        }
        "claim" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            live_worktree(svc, &b, &n, &project, slug)?;
            // Expiry is judged on how long this daemon has watched the claim sit unchanged, on its
            // own monotonic clock; the stored stamp (some caller's wall clock) decides nothing.
            let h = {
                let mut w = claim_watch().lock().map_err(|_| "claim watch poisoned".to_string())?;
                claim_merge_observed(&b, &n.wt, &who(&p), &mut w, mono_ms(), now_ms).map_err(e)?
            };
            refresh(&b, &n.reg, slug, &n.wt, sign)?;
            Ok(json!({"state": h.state, "epoch": h.epoch,
                      "merger": h.merger.map(|m| format!("{}@{}", m.agent, m.host))}))
        }
        "promote" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            live_worktree(svc, &b, &n, &project, slug)?;
            let on_conflict = match p.on_conflict.as_deref() {
                None | Some("fail") => OnConflict::Fail,
                Some("keep_project") => OnConflict::KeepProject,
                Some("overwrite") => OnConflict::Overwrite,
                Some(o) => return Err(format!("--on-conflict is fail, keep_project or overwrite, not '{o}'")),
            };
            let defaults = PromotionPolicy::default();
            let policy = PromotionPolicy {
                on_conflict,
                max_promote_bytes: match p.max_bytes {
                    Some(0) => None,
                    Some(n) => Some(n),
                    None => defaults.max_promote_bytes,
                },
                ..defaults
            };
            if p.dry_run {
                let pv = weftos_store::promote::preview(&b, &n.wt, &n.pr, &policy).map_err(e)?;
                return Ok(json!({"dry_run": true, "preview": pv}));
            }
            // A caller that names the epoch it claimed under is refused when the claim was taken over
            // since (fencing); without one, a land that went through git is the merge whoever held
            // the claim (a claim that is someone else's or has expired must not stop the assets
            // reaching the project). A worktree that was never published has no handoff and gets none.
            if let Some(ep) = p.epoch
                && let Some(h) = weftos_store::share::read_handoff(&b, &n.wt).map_err(e)?
                && h.state == weftos_store::share::State::Claimed
                && h.epoch != ep
            {
                return Err(format!("claim epoch {ep} is stale (now {}); the claim was taken over", h.epoch));
            }
            weftos_store::share::mark_landed(&b, &n.wt, &who(&p), "landed", now_ms).map_err(e)?;
            svc.ensure_archive().map_err(|e| e.to_string())?;
            let prefix = archive_key(&n.wt, now_ms, "");
            let r = promote(
                &b,
                &n.wt,
                &n.pr,
                &ArchiveTarget { bucket: ARCHIVE_BUCKET, prefix: &prefix },
                &policy,
                now_ms,
            )
            .map_err(e)?;
            // Followers must stop seeing a landed worktree as claimed (or open): the entry now says
            // merged, and stays until the worktree store is archived and deleted (see `forget`).
            refresh(&b, &n.reg, slug, &n.wt, sign)?;
            Ok(json!({
                "promoted": r.promoted, "deleted": r.deleted, "kept_project": r.kept_project,
                "archived": r.archived.len(), "dropped_tombstones": r.dropped_tombstones,
            }))
        }
        "orphan" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            orphan::mark(svc, &b, &n, slug, p.since.ok_or("missing --since (ms since the epoch)")?, now_ms, sign)
        }
        "usage" => {
            let n = names(mesh, &project, "x")?;
            let (pobj, pbytes) = weftos_store::retention::usage(&b, &n.pr).map_err(e)?;
            let quota = p.quota_bytes.unwrap_or(weftos_store::retention::DEFAULT_WORKTREE_QUOTA);
            let mut wts = Vec::new();
            for info in worktree_stores(svc, mesh, &project) {
                let landed = weftos_store::promote::is_promoted(&b, &info.bucket).map_err(e)?.map(|r| r.promoted_ms);
                wts.push(json!({
                    "store": info.name, "objects": info.objects, "bytes": info.bytes,
                    "landed_ms": landed, "over_quota": quota > 0 && info.bytes > quota,
                }));
            }
            Ok(json!({"project": {"objects": pobj, "bytes": pbytes}, "worktrees": wts, "quota_bytes": quota}))
        }
        "gc" => {
            use weftos_store::retention::{DAY_MS, DEFAULT_LANDED_KEEP_DAYS, expired_archive_keys, landed_before};
            let orph = orphan::gc(svc, &b, mesh, &project, &names(mesh, &project, "x")?.reg, now_ms, p.confirmed, registry_store)?;
            let keep = p.keep_days.unwrap_or(DEFAULT_LANDED_KEEP_DAYS);
            let cutoff = now_ms.saturating_sub(keep.saturating_mul(DAY_MS));
            let mut retire = Vec::new();
            for info in worktree_stores(svc, mesh, &project) {
                if landed_before(&b, &info.bucket, cutoff).map_err(e)?.is_some() {
                    retire.push(info);
                }
            }
            let mut expired = Vec::new();
            if let Some(days) = p.archive_keep_days {
                let prefix = format!("w-{}-", project.to_lowercase());
                let cutoff = now_ms.saturating_sub(days.saturating_mul(DAY_MS));
                svc.ensure_archive().map_err(|x| x.to_string())?;
                expired = expired_archive_keys(&b, ARCHIVE_BUCKET, &prefix, cutoff).map_err(e)?;
            }
            let names: Vec<String> = retire.iter().map(|i| i.name.clone()).collect();
            if !p.confirmed {
                return Ok(orphan::merged(json!({"dry_run": true, "retire": names, "expire_archived": expired.len(),
                                 "keep_days": keep, "note": "nothing was changed; pass --confirmed to act"}), orph));
            }
            let (mut retired, mut failed) = (Vec::new(), Vec::new());
            for info in &retire {
                let r = StoreName::parse(&info.name).map_err(|x| x.to_string()).and_then(|nm| {
                    svc.archive(&nm, true, "gc").map_err(|x| x.to_string())?;
                    // the bucket is gone: drop its registry entry too (ADR-126 4a), as `store archive --delete` does
                    forget(svc, &nm).map_err(|x| format!("archived, but the registry entry stays: {x}"))
                });
                match r {
                    Ok(_) => retired.push(info.name.clone()),
                    Err(x) => failed.push(json!({"store": info.name, "error": x})),
                }
            }
            for k in &expired {
                weftos_store::overlay::ObjectBackend::delete(&b, ARCHIVE_BUCKET, k).map_err(e)?;
            }
            Ok(orphan::merged(json!({"dry_run": false, "retired": retired, "failed": failed, "expired_archived": expired.len(), "keep_days": keep}), orph))
        }
        "follow" => {
            let n = names(mesh, &project, "x")?;
            let sp = guard.check(need(&p.state, "state")?, "state", Use::WriteFile)?;
            let mut st = FollowState::load(&sp).map_err(e)?;
            let ch = match &sign.trusted {
                // With a trusted-key file every entry is read and verified; the digest shortcut would skip that.
                Some(t) => poll_verified_settled(&b, &n.reg, &mut st, &Trusted(t), sign.require, p.confirmed, sign.follow_retry_ms),
                None => follower::poll_fast(&b, &n.reg, &mut st, p.confirmed, !p.full, share_fetch::FETCH_JOBS),
            }
            .map_err(e)?;
            st.save(&sp).map_err(e)?;
            Ok(json!({"changes": ch.iter().map(change_json).collect::<Vec<_>>(), "trust": trust_json(&b, &n.reg, sign)}))
        }
        "search" => {
            let n = names(mesh, &project, "x")?;
            // One corrupt or unknown-schema registry entry must not blind the search (it does not
            // blind `follow` or `leftovers`): search the readable ones and name the others.
            let (entries, bad_entries) = trusted_entries(&b, &n.reg, sign).map_err(e)?;
            let r = follower::search_docs(&b, &entries, need(&p.query, "query")?);
            Ok(json!({
                "hits": r.hits.iter().map(|h| json!({"slug": h.slug, "path": h.path, "title": h.title, "state": h.state})).collect::<Vec<_>>(),
                "unreadable": r.unreadable,
                "unreadable_entries": bad_entries,
                "trust": trust_json(&b, &n.reg, sign),
            }))
        }
        "leftovers" => {
            let n = names(mesh, &project, "x")?;
            let entries = trusted_entries(&b, &n.reg, sign).map_err(e)?.0;
            let l = follower::leftovers(&entries, now_ms, &LeftoverPolicy::default());
            Ok(json!({"leftovers": l.iter().map(|x| json!({"why": why_str(x.why), "idle_ms": x.idle_ms, "worktree": entry_json(&x.entry)})).collect::<Vec<_>>(),
                      "trust": trust_json(&b, &n.reg, sign)}))
        }
        "browse" => {
            // Read-only view of a shared tree (WO-069 lane B): no slug lists worktrees, a slug lists
            // its root or the folder `--path D/`, and `--path F` reads one file under `--max-bytes`.
            let n = names(mesh, &project, "x")?;
            let trusted = |slug: &str| -> Result<(), String> {
                // With a trusted-key file only a verified registry entry may be browsed.
                if sign.trusted.is_some() && !trusted_entries(&b, &n.reg, sign).map_err(e)?.0.iter().any(|x| x.slug == slug) {
                    return Err(format!("{slug} is not a trusted registry entry"));
                }
                Ok(())
            };
            let to_json = |r: Result<Value, serde_json::Error>| r.map_err(|x| x.to_string());
            let Some(slug) = p.slug.as_deref().filter(|s| !s.is_empty()) else {
                let mut r = follower::browse_registry(&b, &n.reg).map_err(e)?;
                if sign.trusted.is_some() {
                    let ok: Vec<String> = trusted_entries(&b, &n.reg, sign).map_err(e)?.0.into_iter().map(|x| x.slug).collect();
                    r.worktrees.retain(|w| ok.contains(&w.slug));
                }
                return to_json(serde_json::to_value(r));
            };
            trusted(slug)?;
            match p.path.as_deref().filter(|f| !f.is_empty() && !f.ends_with('/')) {
                None => to_json(serde_json::to_value(follower::browse_ls(&b, &n.reg, slug, p.path.as_deref().unwrap_or("")).map_err(e)?)),
                Some(f) => {
                    let cap = p.max_bytes.unwrap_or(weftos_store::follower_browse::MAX_GET_BYTES);
                    let mut r = follower::browse_get(&b, &n.reg, slug, f, cap).map_err(e)?;
                    let text = r.data.take().and_then(|d| String::from_utf8(d).ok());
                    let mut v = to_json(serde_json::to_value(&r))?;
                    v["text"] = text.map_or(Value::Null, Value::String);
                    Ok(v)
                }
            }
        }
        "sync-docs" => {
            let slug = need(&p.slug, "slug")?;
            let n = names(mesh, &project, slug)?;
            let entry = summarize(&b, slug, &n.wt).map_err(e)?.ok_or("worktree has nothing published")?;
            let r = follower::sync_docs(&b, &entry, &guard.check(need(&p.cache, "cache")?, "cache", Use::WriteDir)?).map_err(e)?;
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
                // On different bases a path can differ only because the base moved: discount it.
                "reliable": r.same_base,
                "base_a": r.base_a, "base_b": r.base_b,
                "overlaps": r.overlaps.iter().map(|o| json!({"path": o.path, "kind": format!("{:?}", o.kind)})).collect::<Vec<_>>(),
            }))
        }
        other => Err(format!("unknown worktree verb: {other}")),
    }
}
