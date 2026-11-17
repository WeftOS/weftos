//! `weaver cog checkout release | renew | list` on the daemon (ADR-106
//! phase 3, the remainder).
//!
//! | method                          | capability | what it does |
//! |---------------------------------|------------|--------------|
//! | `workload.cog.checkout.release` | Admin      | `{cog_id, version}`: the Seed withdraws that checkout; the withdrawal is installed and flooded |
//! | `workload.cog.checkout.renew`   | Admin      | `{cog_id, version}`: an on-demand renewal pass now; reports that checkout's grant before and after |
//! | `workload.cog.checkout.list`    | Read       | held grants (validity, expiry, approval per artifact) and approvals |
//!
//! Release and renew are paced node-wide (one per 60 s, `[rate_limited]`
//! with the wait), and arrive as Admin extension routes so the chain events
//! carry the caller's principal. They go through the steward's own link to `weft-licence`
//! (the renewer's client, refusing `not_holder` / `not_steward` without
//! sending), so they run on the steward. The renewal endpoint renews every
//! active checkout of the mesh at once: the Seed has no per-checkout renewal
//! (ADR-106 section 4 defines none), so `renew <cog>@<version>` makes one
//! pass and reports the checkout asked for. Each is chained as
//! `cog.checkout.release` / `cog.checkout.renew` with its outcome, and the
//! renewer chains each grant it installs (`cog.checkout.renewed`,
//! `cog.checkout.lapsed`).

use clawft_kernel::licence::{CheckoutGrant, CheckoutWire, RenewalReport, Renewer};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::licence_boot;
use crate::licence_checkout_rpc::Ctx;

/// Least time between two manual renew or release calls on this node (each
/// costs the Seed a signature per active checkout).
pub const MANUAL_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The node-wide pace of manual renew and release.
pub struct ManualLimit {
    min: std::time::Duration,
    last: std::sync::Mutex<Option<std::time::Instant>>,
}

impl ManualLimit {
    /// A limit of one call per `min`.
    pub const fn new(min: std::time::Duration) -> Self {
        Self { min, last: std::sync::Mutex::new(None) }
    }

    /// Take the slot, or say how long to wait.
    pub fn take(&self) -> Result<(), std::time::Duration> {
        let now = std::time::Instant::now();
        let mut last = self.last.lock().expect("last lock poisoned");
        if let Some(t) = *last {
            let since = now.duration_since(t);
            if since < self.min {
                return Err(self.min - since);
            }
        }
        *last = Some(now);
        Ok(())
    }
}

/// The daemon's limit.
pub static MANUAL: ManualLimit = ManualLimit::new(MANUAL_MIN_INTERVAL);

fn paced(ctx: &Ctx<'_>) -> Result<(), String> {
    ctx.manual.take().map_err(|wait| {
        format!("[rate_limited] one manual renew or release per {} s on this node: try again in {} s",
            MANUAL_MIN_INTERVAL.as_secs(), wait.as_secs() + 1)
    })
}

/// Methods served here.
pub const METHODS: &[&str] =
    &["workload.cog.checkout.release", "workload.cog.checkout.renew", "workload.cog.checkout.list"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OneParams {
    cog_id: String,
    version: String,
}

fn chain(ctx: &Ctx<'_>, kind: &str, payload: Value) {
    ctx.rt.chain.append(licence_boot::LICENCE_CHAIN_SOURCE, kind, Some(payload));
}

fn report_json(r: &RenewalReport) -> Value {
    json!({ "renewed": r.renewed, "withdrawn": r.withdrawn, "unchanged": r.unchanged, "refused": r.refused })
}

/// The held grant for `cog` `version`: `(seq, grant)`.
fn held(ctx: &Ctx<'_>, cog: &str, version: &str) -> Option<(u64, CheckoutGrant)> {
    let (seq, s) = ctx.rt.store().held_grant(cog, version)?;
    serde_json::from_str(&s.payload).ok().map(|g| (seq, g))
}

/// Who may run release or renew here, and with which renewer.
fn steward_renewer<'a>(ctx: &'a Ctx<'_>, p: &OneParams) -> Result<&'a Renewer, String> {
    if let Some(why) = licence_boot::holder_refusal() {
        return Err(why);
    }
    let shape = CheckoutWire { request_id: "r".into(), cog_id: p.cog_id.clone(), version: p.version.clone(), arch: "a".into() };
    if shape.validate().is_err() || p.version == "latest" {
        return Err("give <cog>@<version> with an exact version".into());
    }
    let b = ctx
        .rt
        .store()
        .active_binding()
        .ok_or("[seed_not_bound] no Seed binding is in effect on this node (weaver workload node status)")?;
    if b.steward_node_id != ctx.rt.steward_node_id {
        return Err(format!(
            "[not_steward] the steward is {}: run this there (it holds the link to weft-licence)",
            b.steward_node_id
        ));
    }
    ctx.renewer.as_deref().ok_or_else(|| {
        "[no_steward] this node runs no steward relay: configure licence-link.json and restart".to_string()
    })
}

fn refused(e: &clawft_kernel::licence::LicenceClientError) -> String {
    match e {
        clawft_kernel::licence::LicenceClientError::Refused { code, .. } => format!("[{code}] {e}"),
        other => format!("[licence_unreachable] {other}"),
    }
}

/// `workload.cog.checkout.release`.
pub async fn release(ctx: &Ctx<'_>, p: OneParams) -> Result<Value, String> {
    let renewer = steward_renewer(ctx, &p)?;
    paced(ctx)?;
    let before = held(ctx, &p.cog_id, &p.version).map(|(s, _)| s);
    let out = renewer.release(&p.cog_id, &p.version).await;
    let base = json!({ "cog_id": p.cog_id, "version": p.version, "principal": ctx.principal, "seq_before": before });
    let rep = match out {
        Ok(r) if r.skipped => Err("[not_holder] nothing was sent: this node may not use the licence link now".to_string()),
        Ok(r) => Ok(r),
        Err(e) => Err(refused(&e)),
    };
    let after = held(ctx, &p.cog_id, &p.version);
    let withdrawn = after.as_ref().is_some_and(|(_, g)| g.is_withdrawal());
    let mut ev = base.clone();
    match &rep {
        Ok(r) => {
            ev["outcome"] = json!(if withdrawn { "released" } else { "not_held_by_seed" });
            ev["report"] = report_json(r);
            ev["seq"] = json!(after.as_ref().map(|(s, _)| *s));
        }
        Err(e) => ev["outcome"] = json!(format!("refused: {e}")),
    }
    chain(ctx, "cog.checkout.release", ev);
    let r = rep?;
    if !withdrawn {
        return Err(format!(
            "the Seed returned no withdrawal for {}@{}: it holds no checkout of it for this mesh",
            p.cog_id, p.version
        ));
    }
    Ok(json!({ "released": true, "cog_id": p.cog_id, "version": p.version,
               "seq": after.map(|(s, _)| s), "report": report_json(&r) }))
}

/// `workload.cog.checkout.renew`.
pub async fn renew(ctx: &Ctx<'_>, p: OneParams) -> Result<Value, String> {
    let renewer = steward_renewer(ctx, &p)?;
    let Some((seq_before, _)) = held(ctx, &p.cog_id, &p.version) else {
        return Err(format!("no checkout of {}@{} is held here: weaver cog checkout it first", p.cog_id, p.version));
    };
    paced(ctx)?;
    let out = renewer.run_once().await;
    let after = held(ctx, &p.cog_id, &p.version);
    let mut ev = json!({ "cog_id": p.cog_id, "version": p.version, "principal": ctx.principal, "seq_before": seq_before });
    let rep = match out {
        Ok(r) if r.skipped => Err("[not_holder] nothing was sent: this node may not use the licence link now".to_string()),
        Ok(r) => Ok(r),
        Err(e) => Err(refused(&e)),
    };
    match &rep {
        Ok(r) => {
            ev["outcome"] = json!("renewed");
            ev["report"] = report_json(r);
            ev["seq"] = json!(after.as_ref().map(|(s, _)| *s));
        }
        Err(e) => ev["outcome"] = json!(format!("refused: {e}")),
    }
    chain(ctx, "cog.checkout.renew", ev);
    let r = rep?;
    let (seq, g) = after.ok_or("the grant vanished during the renewal")?;
    Ok(json!({
        "cog_id": p.cog_id, "version": p.version, "seq_before": seq_before, "seq": seq,
        "renewed": seq > seq_before, "withdrawn": g.is_withdrawal(),
        "expires_at": g.expires_at, "report": report_json(&r),
    }))
}

/// `workload.cog.checkout.list`: what this node holds, read-only.
pub fn list(ctx: &Ctx<'_>) -> Value {
    let store = ctx.rt.store();
    let now = store.effective_now().unwrap_or(ctx.now);
    let approvals = ctx.exchange.as_ref().map(|x| x.approvals().clone());
    let grants: Vec<Value> = store
        .grant_rows()
        .into_iter()
        .map(|g| {
            let arts: Vec<Value> = g
                .artifacts
                .iter()
                .map(|a| {
                    let approved = approvals.as_ref().and_then(|ap| ap.covering(&g.cog_id, &g.version, &a.sha256));
                    json!({ "arch": a.arch, "sha256": a.sha256, "approval_id": approved })
                })
                .collect();
            json!({
                "cog_id": g.cog_id, "version": g.version, "seq": g.seq, "valid": g.valid,
                "withdrawn": g.withdrawn, "expires_at": g.expires_at,
                "expires_in": g.expires_at.saturating_sub(now), "artifacts": arts,
            })
        })
        .collect();
    json!({
        "now": now,
        "holder_state": licence_boot::holder_state_name(),
        "grants": grants,
        "approvals": approvals.map(|a| a.rows()).unwrap_or_default(),
    })
}

#[cfg(test)]
#[path = "licence_checkout_verbs_tests.rs"]
mod tests;
