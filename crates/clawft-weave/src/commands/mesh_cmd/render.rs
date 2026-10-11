//! Human-readable renderers for `weaver mesh status` and `bindings`.

use std::io::Write;

use anyhow::Result;
use clawft_mesh_local::hexser;
use clawft_mesh_service::admin_client::fingerprint;
use serde_json::Value;

pub(super) fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("-")
}

pub(super) fn render_status(w: &mut dyn Write, d: &Value) -> Result<()> {
    let key = hexser::decode::<32>(s(d, "machine_pubkey"));
    writeln!(w, "node        {}", s(d, "node_id"))?;
    if let Some(k) = key {
        writeln!(w, "machine key {}  ({})", fingerprint(&k), s(d, "machine_pubkey"))?;
    }
    writeln!(w, "build       {}   proto {}..={}", s(d, "build_sha"), d["proto"]["min"], d["proto"]["max"])?;
    writeln!(w, "listen      {}   socket {}", s(d, "listen"), s(d, "socket"))?;
    writeln!(
        w,
        "policy      admission {}, bind {}, cluster owner uid {}",
        s(d, "admission"),
        s(d, "bind_policy"),
        d["cluster_owner_uid"].as_u64().map_or("-".to_string(), |u| u.to_string())
    )?;
    writeln!(w, "registered  {}", d["registered"])?;
    for p in d["force_revoked"].as_array().into_iter().flatten() {
        writeln!(
            w,
            "FORCE-REVOKED uid {} (enforced from force-revoked.json; the journal could not record it)",
            p["id"]
        )?;
    }
    for r in d["registrations"].as_array().into_iter().flatten() {
        writeln!(
            w,
            "  - uid {} user {} pid {} {}  delivered {} dropped {}",
            r["principal"]["id"], s(r, "user_id"), r["pid"], s(r, "exe"), r["delivered"], r["dropped_full"]
        )?;
    }
    let peers: Vec<&str> = d["peers"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    writeln!(w, "peers       {}", if peers.is_empty() { "none".to_string() } else { peers.join(", ") })?;
    let j = &d["journal"];
    writeln!(
        w,
        "journal     seq {} head {}{}{}",
        j["seq"],
        j["hash"].as_str().map_or("-", |h| &h[..h.len().min(16)]),
        if j["read_only"] == true { "  READ-ONLY (quarantined tail: `weaver mesh journal verify --accept-truncate`)" } else { "" },
        j["degraded"].as_str().map_or(String::new(), |why| format!("  DEGRADED: {why}")),
    )?;
    if j["last_auto_accept"].is_object() {
        writeln!(
            w,
            "            the service accepted a crash-torn journal tail on its own (seq {}, at {}); see `weaver mesh journal verify`",
            j["last_auto_accept"]["seq"], j["last_auto_accept"]["at"]
        )?;
    }
    let r = &d["router"];
    writeln!(
        w,
        "router      delivered {} sent {}/{} dropped: scope_required {} unknown_scope {} denied_scope {} no_tenant {} queue_full {}",
        r["delivered"], r["sent_remote"], r["sent_local"], r["scope_required"], r["unknown_scope"],
        r["denied_scope"], r["no_tenant"], r["dropped_full"]
    )?;
    Ok(())
}

pub(super) fn render_bindings(w: &mut dyn Write, d: &Value) -> Result<()> {
    let who = |p: &Value| p["uid"].as_u64().map_or_else(|| p["sid"].to_string(), |u| format!("uid {u}"));
    for b in d["bound"].as_array().into_iter().flatten() {
        writeln!(
            w,
            "bound    {}  user {}  serials {}{}",
            who(&b["principal"]),
            s(b, "user_id"),
            b["serials"],
            if b["registered"] == true { "  (registered)" } else { "" }
        )?;
    }
    for p in d["pending"].as_array().into_iter().flatten() {
        writeln!(w, "pending  {}  user {}  -> `weaver mesh bind approve <uid>`", who(&p["principal"]), s(p, "user_id"))?;
    }
    for r in d["revoked"].as_array().into_iter().flatten() {
        writeln!(w, "revoked  {}", who(r))?;
    }
    for r in d["force_revoked"].as_array().into_iter().flatten() {
        writeln!(w, "FORCE-REVOKED  {}  (journal could not record it; fix the journal, then re-run the revoke)", who(&serde_json::json!({"uid": r["id"]})))?;
    }
    if d["read_only"] == true {
        writeln!(w, "note     the journal is read-only; binds and certificates are refused")?;
    }
    if let Some(why) = d["degraded"].as_str() {
        writeln!(w, "note     bindings are degraded: {why}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_renders_the_important_lines() {
        let d = serde_json::json!({
            "node_id": "n", "machine_pubkey": "ab".repeat(32), "build_sha": "sha", "proto": {"min": 1, "max": 1},
            "listen": "0.0.0.0:9489", "socket": "/s", "admission": "observe", "bind_policy": "tofu",
            "cluster_owner_uid": 501, "registered": 1, "peers": ["p1"],
            "registrations": [{"principal": {"id": 501}, "user_id": "u", "pid": 7, "exe": "/bin/x", "delivered": 2, "dropped_full": 0}],
            "journal": {"seq": 4, "hash": "0123456789abcdef0123", "read_only": true, "degraded": null},
            "router": {"delivered": 1, "sent_remote": 0, "sent_local": 0, "scope_required": 0, "unknown_scope": 0, "denied_scope": 0, "no_tenant": 0, "dropped_full": 0},
        });
        let mut out = Vec::new();
        render_status(&mut out, &d).unwrap();
        let t = String::from_utf8(out).unwrap();
        assert!(t.contains("admission observe") && t.contains("cluster owner uid 501"), "{t}");
        assert!(t.contains("READ-ONLY") && t.contains("p1") && t.contains("uid 501 user u pid 7"), "{t}");
        assert!(t.contains("0123456789abcdef"), "{t}");
    }

    #[test]
    fn bindings_render_pending_with_the_approve_hint() {
        let d = serde_json::json!({
            "bound": [{"principal": {"uid": 501}, "user_id": "u1", "serials": [1, 2], "registered": true}],
            "pending": [{"principal": {"uid": 502}, "user_id": "u2"}],
            "revoked": [{"uid": 503}], "force_revoked": [{"kind": "uid", "id": 504}], "degraded": null, "read_only": false,
        });
        let mut out = Vec::new();
        render_bindings(&mut out, &d).unwrap();
        let t = String::from_utf8(out).unwrap();
        assert!(t.contains("bound    uid 501") && t.contains("(registered)"), "{t}");
        assert!(t.contains("pending  uid 502") && t.contains("bind approve"), "{t}");
        assert!(t.contains("revoked  uid 503"), "{t}");
        assert!(t.contains("FORCE-REVOKED  uid 504"), "{t}");
    }
}
