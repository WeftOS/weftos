//! The daemon's exposure runtime (ADR-120): the shared registry, the
//! `[mesh.expose]` master switch and the compact `resources` report section.
//!
//! ```toml
//! [mesh.expose]
//! enabled = false   # master switch: nothing is announced or served (default true)
//! ```
//!
//! An unreadable or invalid section fails closed: the master switch is off and
//! the reason is logged. The activity log keeps 14 days or 50 MB (defaults
//! pending the owner).

use std::path::Path;
use std::sync::{Arc, OnceLock};

use clawft_kernel::cluster::ClusterMembership;
use clawft_kernel::exposure::registry::now_ms;
use clawft_kernel::exposure::{AuditFn, ExposureRegistry, Retention};
use serde_json::{Map, Value, json};

static GLOBAL: OnceLock<Arc<ExposureRegistry>> = OnceLock::new();
static FACTS: OnceLock<(Arc<ClusterMembership>, String)> = OnceLock::new();

/// The daemon's registry, once started.
pub fn global() -> Option<Arc<ExposureRegistry>> {
    GLOBAL.get().cloned()
}

/// Where `resources` reads this node's facts from (called by node facts init).
pub fn set_facts_source(membership: Arc<ClusterMembership>, node_id: String) -> bool {
    FACTS.set((membership, node_id)).is_ok()
}

/// `[mesh.expose] enabled` of a merged config value (absent: true).
pub fn master_from_config(root: &Value) -> Result<bool, String> {
    let Some(expose) = root.get("mesh").and_then(|m| m.get("expose")) else {
        return Ok(true);
    };
    let Some(obj) = expose.as_object() else {
        return Err("[mesh.expose] must be a table".into());
    };
    if let Some(k) = obj.keys().find(|k| k.as_str() != "enabled") {
        return Err(format!("[mesh.expose] unknown key {k:?}"));
    }
    match obj.get("enabled") {
        None => Ok(true),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err("[mesh.expose] enabled must be true or false".into()),
    }
}

async fn master_for(home: Option<&Path>) -> bool {
    let Some(home) = home else { return true };
    use clawft_platform::{Platform, config_loader};
    let platform = clawft_platform::NativePlatform::new();
    let path = crate::user_daemon::user_weave_toml(home);
    let parsed = match config_loader::load_weave_toml_file(platform.fs(), &path).await {
        Ok(v) => master_from_config(&v),
        Err(e) => Err(format!("{}: {e}", path.display())),
    };
    parsed.unwrap_or_else(|e| {
        tracing::error!(error = %e, "mesh exposure is switched OFF: weave.toml [mesh.expose] could not be read");
        false
    })
}

/// Start the registry under `dir` (idempotent: the first call wins). A broken
/// activity chain is moved aside and logged, never read as good.
pub async fn start(node_id: &str, dir: &Path, home: Option<&Path>, audit: Option<AuditFn>) -> Arc<ExposureRegistry> {
    if let Some(g) = global() {
        return g;
    }
    let ret = Retention::default();
    let reg = match ExposureRegistry::open(node_id, dir, ret, now_ms()) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "exposure state/activity log unreadable; moving it aside and starting fresh");
            for f in ["exposures.json", "exposure-activity.jsonl"] {
                let p = dir.join(f);
                if p.exists() {
                    let _ = std::fs::rename(&p, dir.join(format!("{f}.broken-{}", now_ms())));
                }
            }
            match ExposureRegistry::open(node_id, dir, ret, now_ms()) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "exposure registry kept in memory only");
                    ExposureRegistry::new(node_id)
                }
            }
        }
    };
    let reg = Arc::new(reg);
    if let Some(a) = audit {
        reg.set_audit(a);
    }
    reg.set_master(master_for(home).await);
    GLOBAL.get_or_init(|| reg).clone()
}

fn attr(c: &clawft_types::placement::Capability, k: &str) -> Value {
    c.attrs.get(k).and_then(|v| serde_json::to_value(v).ok()).unwrap_or(Value::Null)
}

/// The compact `resources` section from a node's capabilities: CPU, memory,
/// GPUs/NPUs, VRAM free vs total, and library versions. No paths or names.
pub fn resources_from(caps: &[clawft_types::placement::Capability]) -> Value {
    let mut out = Map::new();
    if let Some(c) = caps.iter().find(|c| c.id.as_str().starts_with("cpu.arch.")) {
        out.insert("cpu".into(), json!({"arch": c.id.as_str().trim_start_matches("cpu.arch."), "cores": attr(c, "cores")}));
    }
    if let Some(c) = caps.iter().find(|c| c.id.as_str() == "mem.unified").or_else(|| caps.iter().find(|c| c.id.as_str() == "mem.system")) {
        out.insert("mem".into(), json!({"kind": c.id.as_str().trim_start_matches("mem."), "total": attr(c, "total"), "free": attr(c, "free")}));
    }
    let gpus: Vec<Value> = caps
        .iter()
        .filter(|c| c.id.as_str().starts_with("accel.gpu."))
        .map(|c| json!({"kind": c.id.as_str().trim_start_matches("accel.gpu."), "model": attr(c, "model"), "device": attr(c, "device"), "cores": attr(c, "cores")}))
        .collect();
    out.insert("gpus".into(), Value::Array(gpus));
    let npus: Vec<Value> = caps
        .iter()
        .filter(|c| c.id.as_str().starts_with("accel.npu.") || c.id.as_str().starts_with("accel.tpu."))
        .map(|c| json!({"kind": c.id.as_str().trim_start_matches("accel."), "device": attr(c, "device")}))
        .collect();
    out.insert("npus".into(), Value::Array(npus));
    let vram: Vec<Value> = caps
        .iter()
        .filter(|c| c.id.as_str() == "mem.vram")
        .map(|c| json!({"device": attr(c, "device"), "total": attr(c, "total"), "free": attr(c, "free")}))
        .collect();
    out.insert("vram".into(), Value::Array(vram));
    let mut libs = Map::new();
    for c in caps.iter().filter(|c| c.id.as_str().starts_with("x.lib.")) {
        let mut m = Map::new();
        for (k, v) in &c.attrs {
            if let Ok(v) = serde_json::to_value(v) {
                m.insert(k.clone(), v);
            }
        }
        libs.insert(c.id.as_str().trim_start_matches("x.lib.").to_owned(), Value::Object(m));
    }
    out.insert("libs".into(), Value::Object(libs));
    Value::Object(out)
}

/// `resources` for the heartbeat report, from this node's cached facts. `None`
/// until the first probe has landed.
pub fn resources_json() -> Option<Value> {
    let (membership, id) = FACTS.get()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let cached = membership.facts().get(id, now)?;
    Some(resources_from(&cached.facts.capabilities))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_types::placement::{Capability, CapabilityId, Provenance};

    fn cap(id: &str) -> Capability {
        Capability::new(CapabilityId::new(id).unwrap(), Provenance::Probed)
    }

    #[test]
    fn master_switch_parses_and_fails_on_garbage() {
        assert_eq!(master_from_config(&json!({})), Ok(true));
        assert_eq!(master_from_config(&json!({"mesh": {}})), Ok(true));
        assert_eq!(master_from_config(&json!({"mesh": {"expose": {}}})), Ok(true));
        assert_eq!(master_from_config(&json!({"mesh": {"expose": {"enabled": false}}})), Ok(false));
        assert_eq!(master_from_config(&json!({"mesh": {"expose": {"enabled": true}}})), Ok(true));
        for bad in [json!({"mesh": {"expose": {"enabled": "no"}}}), json!({"mesh": {"expose": {"enable": false}}}), json!({"mesh": {"expose": true}})] {
            assert!(master_from_config(&bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_weave_toml_section_fails_closed() {
        let dir = std::env::temp_dir().join(format!("expose-home-{}", std::process::id()));
        let w = crate::user_daemon::user_weave_toml(&dir);
        std::fs::create_dir_all(w.parent().unwrap()).unwrap();
        std::fs::write(&w, "[mesh.expose]\nenabled = \"maybe\"\n").unwrap();
        assert!(!master_for(Some(&dir)).await, "a bad section switches exposure off");
        std::fs::write(&w, "[mesh.expose]\nenabled = false\n").unwrap();
        assert!(!master_for(Some(&dir)).await);
        std::fs::write(&w, "[mesh.expose]\n").unwrap();
        assert!(master_for(Some(&dir)).await);
        assert!(master_for(None).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resources_are_compact_and_carry_versions_and_vram() {
        let caps = vec![
            cap("cpu.arch.x86_64").with_attr("cores", 20_i64),
            cap("mem.system").with_attr("total", 128_i64).with_attr("free", 64_i64),
            cap("accel.gpu.cuda").with_attr("model", "GB10").with_attr("device", "cuda:0"),
            cap("mem.vram").with_attr("device", "cuda:0").with_attr("total", 100_i64).with_attr("free", 40_i64),
            cap("accel.npu.hailo").with_attr("device", "hailo0"),
            cap("x.lib.cuda").with_attr("driver", "550.54").with_attr("cudnn", "9"),
            cap("store.tier.internal").with_attr("path", "/home/secret"),
        ];
        let v = resources_from(&caps);
        assert_eq!(v["cpu"], json!({"arch": "x86_64", "cores": 20}));
        assert_eq!(v["mem"]["free"], 64);
        assert_eq!(v["vram"][0], json!({"device": "cuda:0", "total": 100, "free": 40}));
        assert_eq!(v["gpus"][0]["model"], "GB10");
        assert_eq!(v["npus"][0]["kind"], "npu.hailo");
        assert_eq!(v["libs"]["cuda"]["driver"], "550.54");
        assert!(!v.to_string().contains("secret"), "storage paths are not reported");
        assert!(v.to_string().len() < 1024);
    }

    #[test]
    fn unified_memory_wins_over_system_memory() {
        let caps = vec![
            cap("mem.system").with_attr("total", 1_i64),
            cap("mem.unified").with_attr("total", 2_i64).with_attr("free", 1_i64),
        ];
        assert_eq!(resources_from(&caps)["mem"], json!({"kind": "unified", "total": 2, "free": 1}));
    }
}
