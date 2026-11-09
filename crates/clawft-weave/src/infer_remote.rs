//! A role served by a hosted, OpenAI-compatible API (`mode: "remote"`), and
//! the per-role forwarding fields (`stall_timeout_s`, `pin_model`).
//!
//! ```json
//! { "role": "hard-task", "mode": "remote", "proxy_port": 8095,
//!   "remote": { "base_url": "https://api.x.ai/v1", "api_key_env": "GROK_API_KEY",
//!               "model": "grok-4" },
//!   "stall_timeout_s": 300 }
//! ```
//!
//! The key is named, never stored: `api_key_env` is the name of an
//! environment variable of the daemon process. It is read each time a request
//! is forwarded (so a rotated key needs no restart, and the value is never
//! held in config, state or status), sent as the only `Authorization` header
//! (a client's own is dropped) and kept out of every error and record.
//! The destination is `base_url` alone: a request cannot name a host.

use clawft_kernel::infer_proxy::hosted::{parse_hosted_base, valid_env_name};
use clawft_kernel::workload_pkg::manifest::valid_token;
use serde::Deserialize;

use crate::infer_cfg::RoleCfg;

/// Longest stall timeout a role may set, seconds (the default request cap).
pub const MAX_STALL_SECS: u64 = 900;
/// Shortest stall timeout a role may set, seconds.
pub const MIN_STALL_SECS: u64 = 10;

/// A hosted API a role is served from.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteCfg {
    /// API root with its version: `https://api.x.ai/v1`. `https://`, or
    /// `http://` for a loopback IP literal only.
    pub base_url: String,
    /// Name of the daemon environment variable that holds the API key.
    pub api_key_env: String,
    /// Model every request is pinned to.
    pub model: String,
}

impl RemoteCfg {
    /// Boundary validation.
    pub fn validate(&self, role: &str) -> Result<(), String> {
        parse_hosted_base(&self.base_url).map_err(|m| format!("role {role}: remote.{m}"))?;
        if !valid_env_name(&self.api_key_env) {
            return Err(format!("role {role}: remote.api_key_env is the NAME of an environment variable (letters, digits, _)"));
        }
        if !valid_token(&self.model, 128) {
            return Err(format!("role {role}: remote.model is 1..=128 characters of a model id"));
        }
        Ok(())
    }
}

/// Rules for the remote, stall and pin fields of one role.
pub fn validate_role(r: &RoleCfg) -> Result<(), String> {
    let role = &r.role;
    if let Some(s) = r.stall_timeout_s
        && !(MIN_STALL_SECS..=MAX_STALL_SECS).contains(&s)
    {
        return Err(format!("role {role}: stall_timeout_s must be {MIN_STALL_SECS}..={MAX_STALL_SECS}"));
    }
    if r.svc.is_some() && (r.stall_timeout_s.is_some() || r.pin_model) {
        return Err(format!("role {role}: a svc role has no stall_timeout_s or pin_model (the service owns both)"));
    }
    let is_remote = r.mode.as_deref() == Some("remote");
    match (&r.remote, is_remote) {
        (None, false) => return Ok(()),
        (None, true) => return Err(format!("role {role}: mode remote needs a remote block (base_url, api_key_env, model)")),
        (Some(_), false) => return Err(format!("role {role}: a remote block needs mode remote")),
        (Some(c), true) => c.validate(role)?,
    }
    let local_fields = r.flavor.is_some()
        || r.roster_id.is_some()
        || r.model.is_some()
        || r.instance_port.is_some()
        || r.memory_gb.is_some()
        || r.ctx.is_some()
        || r.autostart
        || !r.excludes.is_empty()
        || r.svc.is_some()
        || r.on_occupied.is_some();
    if local_fields {
        return Err(format!("role {role}: a remote role has no flavor, roster_id, model, instance_port, ctx, memory_gb, excludes, on_occupied, autostart or svc (the hosted API owns the model; set remote.model)"));
    }
    if r.expose.is_some() {
        return Err(format!("role {role}: a remote role is never exposed beyond loopback (it spends this node's key)"));
    }
    if r.proxy_port.is_none() {
        return Err(format!("role {role}: a remote role needs a proxy_port"));
    }
    Ok(())
}

/// A remote role is never named in the mesh settings.
pub fn check_mesh(cfg: &crate::infer_cfg::FileCfg) -> Result<(), String> {
    let remote = |role: &String| cfg.roles.iter().any(|r| &r.role == role && r.remote.is_some());
    if cfg.mesh.expose.iter().any(remote) || cfg.mesh.serve_peers.keys().chain(cfg.mesh.remote_nodes.keys()).any(remote) {
        return Err("a remote role is never shared over the mesh (mesh settings name one)".into());
    }
    Ok(())
}

/// Install the role's forwarding policy in the placement table and, for a
/// remote role, its route. A remote role is placed at boot: nothing about it
/// depends on a server coming up.
pub fn apply(r: &crate::infer_cfg::Resolved, table: &clawft_kernel::infer_proxy::PlacementTable) {
    use clawft_kernel::infer_proxy::{HostedRoute, RolePolicy};
    let pin_model = match &r.cfg.remote {
        Some(c) => Some(c.model.clone()),
        None => r.cfg.pin_model.then(|| r.spec.model.clone()).flatten(),
    };
    let stall = r.cfg.stall_timeout_s.map(std::time::Duration::from_secs);
    table.set_role_policy(&r.cfg.role, RolePolicy { stall, pin_model });
    if let Some(c) = &r.cfg.remote {
        let route = HostedRoute { base_url: c.base_url.clone(), api_key_env: c.api_key_env.clone(), model: c.model.clone() };
        if let Err(e) = table.register_hosted(&r.cfg.role, route) {
            tracing::warn!(role = %r.cfg.role, error = %e, "remote role not registered");
        }
    }
}
