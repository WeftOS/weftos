//! Resolving a validated [`FileCfg`] into per-role specs: roster facts merged
//! under each role's own fields (explicit fields win).

use clawft_kernel::workload_runtime::infer::{GB, ImportedRoster, InferFlavor, InferenceSpec};

use crate::infer_cfg::{CONFIG_FILE, FileCfg, RoleCfg, flavor_of};

/// A role with everything resolved: roster facts merged under the role's
/// own fields, and the spec the adapter will see.
#[derive(Debug, Clone)]
pub struct Resolved {
    /// The role's config.
    pub cfg: RoleCfg,
    /// Managed (else adopted).
    pub managed: bool,
    /// The inference spec.
    pub spec: InferenceSpec,
}

/// Merge roster facts (when `imported` is given) under each role's fields
/// and build its spec. Explicit role fields win over the roster.
pub fn resolve_roles(cfg: &FileCfg, imported: Option<&ImportedRoster>) -> Result<Vec<Resolved>, String> {
    let mut out: Vec<Resolved> = Vec::new();
    for r in &cfg.roles {
        let fail = |m: String| Err(format!("{CONFIG_FILE}: role {}: {m}", r.role));
        if let Some(s) = &r.svc {
            // The service owns the model and the server: nothing to place, so
            // the spec is only a label for status.
            let mut spec = InferenceSpec::new(&r.role, InferFlavor::LlamaCpp);
            spec.model = Some(s.model.clone());
            out.push(Resolved { cfg: r.clone(), managed: false, spec });
            continue;
        }
        if let Some(c) = &r.remote {
            // The hosted API owns the model and the server: nothing to place.
            let mut spec = InferenceSpec::new(&r.role, InferFlavor::LlamaCpp);
            spec.model = Some(c.model.clone());
            out.push(Resolved { cfg: r.clone(), managed: false, spec });
            continue;
        }
        let from_roster = match (&r.roster_id, imported) {
            (Some(id), Some(roster)) => match roster.get(id) {
                Some(s) => Some(s.spec.clone()),
                None => {
                    let why = roster.skipped.iter().find(|(i, _)| i == id).map(|(_, w)| w.as_str()).unwrap_or("not in the roster");
                    return fail(format!("roster_id {id}: {why}"));
                }
            },
            _ => None,
        };
        let flavor = match (&r.flavor, &from_roster) {
            (Some(f), _) => flavor_of(f).ok_or("flavor")?,
            (None, Some(s)) => s.runtime,
            (None, None) => return fail("no flavor".into()),
        };
        let mut spec = from_roster.unwrap_or_else(|| InferenceSpec::new(&r.role, flavor));
        spec.role = r.role.clone();
        spec.runtime = flavor;
        if let Some(p) = r.instance_port {
            spec.serve.port = Some(p);
        }
        if spec.serve.port.is_none() {
            return fail("no instance_port (and the roster has none)".into());
        }
        if r.proxy_port.is_some() && r.proxy_port == spec.serve.port {
            return fail("proxy_port equals the server's port: give the server its own instance_port".into());
        }
        if let Some(m) = &r.model {
            spec.model = Some(m.clone());
        }
        if let Some(g) = r.memory_gb {
            spec.memory.weights_bytes = (g * GB as f64) as u64;
        }
        if !r.excludes.is_empty() {
            spec.excludes = r.excludes.clone();
        }
        if let Some(l) = r.latency_class {
            spec.latency_class = l;
        }
        if let Some(s) = r.sticky {
            spec.sticky = s;
        }
        if let Some(c) = r.ctx {
            spec.serve.ctx = Some(c);
        }
        let managed = r.mode.as_deref() == Some("managed");
        if r.pin_model && spec.model.is_none() {
            return fail("pin_model needs a model (for mlx-lm, the name the server loaded or default_model)".into());
        }
        if managed && spec.model.is_none() {
            return fail("a managed role needs a model (registry name)".into());
        }
        if managed && cfg.budget_gb.is_some() && spec.memory.weights_bytes == 0 {
            return fail("a managed role needs its memory (memory_gb or the roster's ram_gb) when budget_gb is set".into());
        }
        spec.validate().map_err(|e| format!("{CONFIG_FILE}: role {}: {e}", r.role))?;
        // Ports are known now (a roster may have supplied them): no two
        // roles share a server port unless both are Ollama, and a server
        // never sits on a listener's port.
        let port = spec.serve.port;
        let listener = |p: Option<u16>| p.is_some() && p == port;
        for other in &out {
            if other.spec.serve.port == port
                && !(other.spec.runtime == InferFlavor::Ollama && spec.runtime == InferFlavor::Ollama)
            {
                return fail(format!("port {:?} is also role {}'s server port (only Ollama roles share a server)", port, other.cfg.role));
            }
            if listener(other.cfg.proxy_port) || listener(other.cfg.expose.as_ref().map(|e| e.port)) {
                return fail(format!("server port {port:?} is role {}'s listener port", other.cfg.role));
            }
        }
        if cfg.roles.iter().any(|o| o.role != r.role && (listener(o.proxy_port) || listener(o.expose.as_ref().map(|e| e.port)))) {
            return fail(format!("server port {port:?} is another role's listener port"));
        }
        out.push(Resolved { cfg: r.clone(), managed, spec });
    }
    Ok(out)
}
