//! Views and name helpers of `store.worktree.*`: store and bucket names, the JSON shape of registry
//! entries and changes, and the registry readers the verbs share. A child of `store_worktree` so it
//! shares that module's private types; the parent imports every item here.

use super::*;

pub(super) fn names(mesh: &str, project: &str, slug: &str) -> Result<Names, String> {
    names_with(mesh, project, slug, false)
}

pub(super) fn names_with(mesh: &str, project: &str, slug: &str, registry_store: bool) -> Result<Names, String> {
    let e = |e: weftos_store::StoreError| e.to_string();
    let wt_name = StoreName::worktree(mesh, project_ok(project)?, slug_ok(slug)?).map_err(e)?;
    let pr_name = StoreName::project(mesh, project).map_err(e)?;
    let reg_name = if registry_store {
        StoreName::release(mesh, project, REGISTRY_CHANNEL).map_err(e)?
    } else {
        pr_name.clone()
    };
    Ok(Names { wt: wt_name.bucket(), pr: pr_name.bucket(), reg: reg_name.bucket(), wt_name, pr_name })
}

pub(super) fn entry_json(e: &RegistryEntry) -> Value {
    json!({
        "slug": e.slug, "branch": e.branch, "head": e.head_commit, "state": e.state,
        "writer": format!("{}@{}", e.writer.agent, e.writer.host),
        "merger": e.merger.as_ref().map(|m| format!("{}@{}", m.agent, m.host)),
        "docs": e.doc_count, "files": e.file_count, "updated_ms": e.updated_ms,
    })
}

pub(super) fn change_json(c: &Change) -> Value {
    match c {
        Change::New(e) => json!({"change": "new", "worktree": entry_json(e)}),
        Change::HeadMoved { entry, from } => json!({"change": "head_moved", "from": from, "worktree": entry_json(entry)}),
        Change::StateChanged { entry, from } => json!({"change": "state_changed", "from": from, "worktree": entry_json(entry)}),
        Change::Gone { slug } => json!({"change": "gone", "slug": slug}),
        Change::Unreadable { slug } => json!({"change": "unreadable", "slug": slug}),
    }
}

pub(super) fn why_str(w: Why) -> &'static str {
    match w {
        Why::ForgottenOpen => "forgotten_open",
        Why::UnclaimedHandoff => "unclaimed_handoff",
        Why::AbandonedClaim => "abandoned_claim",
        Why::StuckCleanup => "stuck_cleanup",
        Why::Orphan => "orphan",
        Why::FutureTimestamp => "future_timestamp",
    }
}

/// A verb that writes into a worktree must find a live, matching worktree (review F6, F8): not
/// archived or closed, created (so its metadata names this project and slug, which also stops a
/// slug whose 48-bit bucket hash collides with another from writing into that other's bucket).
/// A store another daemon created is fine: it need not be known here, only be a real worktree.
pub(super) fn live_worktree(svc: &StoreService, b: &Op, n: &Names, project: &str, slug: &str) -> Result<(), String> {
    for name in [&n.wt_name, &n.pr_name] {
        if svc.is_archived(name) == Some(true) {
            return Err(format!("{name} is archived; refusing"));
        }
    }
    let meta = match read_meta(b, &n.wt) {
        Ok(m) => m,
        Err(weftos_store::overlay::S2Error::Backend(m)) if m.contains("no such bucket") => None,
        Err(e) => return Err(e.to_string()),
    };
    match meta {
        None => Err(format!("worktree {slug} was never created here (no metadata); run create first")),
        Some(m) if m.slug == slug && m.project.eq_ignore_ascii_case(project) => Ok(()),
        Some(m) => Err(format!("bucket {} belongs to worktree {} of project {}, not {slug}", n.wt, m.slug, m.project)),
    }
}

/// Re-derive one worktree's registry entry from its own bucket (the daemon is the only writer).
pub(super) fn refresh(b: &Op, pr: &str, slug: &str, wt: &str, sign: &SignCfg) -> Result<(), String> {
    refresh_with(b, pr, slug, wt, sign, None, None)
}

/// `refresh` with the manifest and/or handoff the caller already holds and knows are current (a
/// publish just wrote them): each `Some` saves its read, each `None` is read as `summarize` does.
pub(super) fn refresh_with(
    b: &Op,
    pr: &str,
    slug: &str,
    wt: &str,
    sign: &SignCfg,
    manifest: Option<weftos_store::share::TreeManifest>,
    handoff: Option<weftos_store::share::Handoff>,
) -> Result<(), String> {
    let e = |e: weftos_store::overlay::S2Error| e.to_string();
    let m = match manifest {
        Some(m) => Some(m),
        None => weftos_store::share::read_manifest(b, wt).map_err(e)?,
    };
    let h = match handoff {
        Some(h) => Some(h),
        None => weftos_store::share::read_handoff(b, wt).map_err(e)?,
    };
    if let (Some(m), Some(h)) = (m, h) {
        let ent = weftos_store::share::summarize_with(slug, wt, m, h);
        match &sign.key {
            Some(k) => write_registry_entry_signed(b, pr, &ent, &Ed(k.clone())).map_err(e)?,
            None => write_registry_entry(b, pr, &ent).map_err(e)?,
        }
    }
    Ok(())
}

/// This project's worktree stores on the node.
pub(super) fn worktree_stores(svc: &StoreService, mesh: &str, project: &str) -> Vec<weftos_store::StoreInfo> {
    let marker = format!("weftos://{mesh}/projects/{project}/worktrees/");
    svc.list_stores().into_iter().filter(|i| i.kind == "worktree" && i.name.to_lowercase().starts_with(&marker.to_lowercase())).collect()
}

/// The registry's entries as this follower should see them: all of them, or with a trusted-key
/// file the ones that verify (all of them when `REQUIRE_SIGNED` is off).
pub(super) fn trusted_entries(b: &Op, reg: &str, sign: &SignCfg) -> weftos_store::overlay::Result<(Vec<RegistryEntry>, Vec<String>)> {
    Ok(match &sign.trusted {
        Some(t) => {
            let (v, bad) = list_trusted_lenient(b, reg, &Trusted(t), sign.require)?;
            (v.into_iter().map(|(e, _)| e).collect(), bad)
        }
        None => list_shared_lenient(b, reg)?,
    })
}

/// Each listed entry's trust, for verb output (`{}` when no trusted-key file is set).
pub(super) fn trust_json(b: &Op, reg: &str, sign: &SignCfg) -> Value {
    let Some(t) = &sign.trusted else { return json!({}) };
    match list_trusted_gen(b, reg, &Trusted(t), false) {
        Ok((v, bad)) => {
            let mut m = serde_json::Map::new();
            for (e, tr, _) in v {
                let name = match tr {
                    weftos_store::registry_sign::Trust::Verified => "verified",
                    weftos_store::registry_sign::Trust::Unsigned => "unsigned",
                    weftos_store::registry_sign::Trust::Bad => "bad",
                };
                m.insert(e.slug, json!(name));
            }
            for s in bad {
                m.insert(s, json!("refused"));
            }
            Value::Object(m)
        }
        Err(e) => json!({"error": e.to_string()}),
    }
}
