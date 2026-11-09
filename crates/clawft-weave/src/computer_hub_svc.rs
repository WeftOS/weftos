//! The Tapestry computer hub as a daemon service (owner decision D1, 2026-11-09).
//!
//! The hub itself is the `weft-computer-hub` library (`weft_hub`): a WebSocket
//! rendezvous that Tapestry's workspace server and harness connect through.
//! This module hosts it inside the daemon, one per mesh, and owns the one thing
//! the library leaves to the host: who may mint pass keys.
//!
//! * **Start.** The service runs only when `<runtime dir>/computer-hub/hub.toml`
//!   exists. The runtime dir is the daemon's own (`WEFTOS_RUNTIME_DIR` honoured),
//!   passed in by the caller; nothing here reads `~/.weftos` by itself. Minter
//!   key files that are missing are created 0600 on first start. A present but
//!   broken file is an error, never a silent "no hub".
//! * **Machines are data.** `[[machines]]` and `[[auth.minters]]` in that file
//!   name the first-class desktops (D2). `host_kind` is assigned by the minter a
//!   machine names; no caller input chooses it.
//! * **RPC** (registered in `rpc_ext::ROUTES`, one route per method):
//!
//!   | Method | Capability | Does |
//!   |---|---|---|
//!   | `computer.hub.status` | Read | listen address, machines (minter, host_kind, connected), connection count |
//!   | `computer.hub.mint` | Admin | mint a pass key for a machine or a harness |
//!   | `computer.hub.revoke` | Admin | refuse a pass key (by `jti`) from now on |
//!
//!   Minting is Admin because a pass key is the credential for reaching a
//!   desktop. The token is returned to the Admin caller once and is never
//!   logged; the hub keeps only the minter keys, never issued tokens.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use clawft_rpc::Response;
use serde_json::{Value, json};
use weft_hub::config::HubConfig;
use weft_hub::hub::{Hub, now_secs};
use weft_hub::passkey::{self, Role};
use weft_hub::server;

use crate::rpc_ext::{ExtCall, ExtFuture};

/// Directory under the runtime dir that holds the hub's config and keys.
pub const CONFIG_DIR: &str = "computer-hub";
/// The config file whose presence turns the service on.
pub const CONFIG_FILE: &str = "hub.toml";
/// Default pass-key lifetime when the caller names none (seconds).
pub const DEFAULT_TTL_SECS: u64 = 3600;
/// Longest subject accepted.
const MAX_SUB_LEN: usize = 128;

/// The running service.
pub struct ComputerHubService {
    hub: Arc<Hub>,
    addr: SocketAddr,
    config_path: PathBuf,
    stop: tokio::sync::watch::Sender<bool>,
}

static GLOBAL: OnceLock<Arc<ComputerHubService>> = OnceLock::new();

/// The daemon's hub, once started. `None` on a node that does not host one.
pub fn global() -> Option<Arc<ComputerHubService>> {
    GLOBAL.get().cloned()
}

/// Where the hub's config lives under `runtime_dir`.
pub fn config_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(CONFIG_DIR).join(CONFIG_FILE)
}

/// Start the hub if `<runtime_dir>/computer-hub/hub.toml` exists, bind its
/// listener and serve in the background. `Ok(None)` means "not configured on
/// this node". Refuses a non-loopback bind the file did not opt into.
pub async fn start(runtime_dir: &Path) -> Result<Option<Arc<ComputerHubService>>, String> {
    let path = config_path(runtime_dir);
    if !path.exists() {
        return Ok(None);
    }
    let cfg = HubConfig::load_ensuring_keys(&path).map_err(|e| e.to_string())?;
    let sa = server::check_bind(&cfg.addr, cfg.allow_non_loopback)?;
    let listener = tokio::net::TcpListener::bind(sa)
        .await
        .map_err(|e| format!("computer hub: bind {sa}: {e}"))?;
    let addr = listener.local_addr().map_err(|e| format!("computer hub: {e}"))?;
    let machines = cfg.machines.len();
    let hub = Hub::new(cfg);
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    let serving = hub.clone();
    tokio::spawn(async move {
        server::serve(serving, listener, async move {
            let _ = stopped.wait_for(|s| *s).await;
        })
        .await;
    });
    tracing::info!(%addr, machines, "computer hub started");
    Ok(Some(Arc::new(ComputerHubService {
        hub,
        addr,
        config_path: path,
        stop,
    })))
}

/// [`start`] and keep the result as the daemon's global hub. A second call
/// while one is running is refused, so a node never serves two.
pub async fn start_global(runtime_dir: &Path) -> Result<Option<Arc<ComputerHubService>>, String> {
    if GLOBAL.get().is_some() {
        return Err("computer hub already started on this node".into());
    }
    let svc = start(runtime_dir).await?;
    if let Some(s) = &svc {
        let _ = GLOBAL.set(s.clone());
    }
    Ok(svc)
}

impl ComputerHubService {
    /// The address actually bound (useful when the config asks for port 0).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The hub, for in-process callers and tests.
    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    /// Stop accepting connections.
    pub fn shutdown(&self) {
        let _ = self.stop.send(true);
    }

    /// `computer.hub.status`.
    pub fn status(&self) -> Value {
        let cfg = self.hub.config();
        let connected = self.hub.connected_servers();
        let mut machines: Vec<_> = cfg.machines.values().collect();
        machines.sort_by(|a, b| a.id.cmp(&b.id));
        let rows: Vec<Value> = machines
            .into_iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "description": m.description,
                    "minter": m.minter,
                    "host_kind": cfg.minters.get(&m.minter).map(|x| x.host_kind.as_str()),
                    "connected": connected.iter().any(|c| c == &m.id),
                })
            })
            .collect();
        json!({
            "running": true,
            "addr": self.addr.to_string(),
            "ws_path": server::WS_PATH,
            "config": self.config_path.display().to_string(),
            "max_ttl_secs": cfg.max_ttl_secs,
            "connections": self.hub.connection_count(),
            "machines": rows,
        })
    }

    /// `computer.hub.mint`. Pure over the hub's config plus the clock `now`.
    pub fn mint(&self, params: &Value, now: u64) -> Result<Value, String> {
        let p = MintParams::parse(params)?;
        let cfg = self.hub.config();
        // The minter is derived, never chosen freely: a machine names its
        // minter, so `host_kind` follows from the machine.
        let minter = match (&p.machine, &p.minter) {
            (Some(m), given) => {
                let derived = cfg
                    .minter_of_machine(m)
                    .ok_or_else(|| format!("machine {m:?} is not configured"))?;
                if let Some(g) = given
                    && g != &derived.id
                {
                    return Err(format!(
                        "machine {m:?} is served by minter {:?}, not {g:?}",
                        derived.id
                    ));
                }
                derived
            }
            (None, Some(g)) => cfg
                .minters
                .get(g)
                .ok_or_else(|| format!("minter {g:?} is not configured"))?,
            (None, None) => {
                return Err("name a `machine`, or a `minter` for a harness-only key".into());
            }
        };
        if p.roles.contains(&Role::Server) && p.machine.is_none() {
            return Err("a server pass key must name its `machine`".into());
        }
        if p.ttl_secs > cfg.max_ttl_secs {
            return Err(format!(
                "ttl_secs {} exceeds the hub's max_ttl_secs {}",
                p.ttl_secs, cfg.max_ttl_secs
            ));
        }
        let token = passkey::mint(minter, &p.sub, &p.roles, p.ttl_secs, p.machine.as_deref(), now)
            .map_err(|e| e.to_string())?;
        // The jti is inside the token; read it back so the caller can revoke.
        let jti = decode_jti(&token).unwrap_or_default();
        Ok(json!({
            "token": token,
            "jti": jti,
            "sub": p.sub,
            "roles": p.roles,
            "machine": p.machine,
            "minter": minter.id,
            "host_kind": minter.host_kind.as_str(),
            "expires_at": now.saturating_add(p.ttl_secs),
            "addr": self.addr.to_string(),
            "ws_path": server::WS_PATH,
        }))
    }

    /// `computer.hub.revoke`.
    pub fn revoke(&self, params: &Value) -> Result<Value, String> {
        reject_unknown(params, &["jti"])?;
        let jti = params
            .get("jti")
            .and_then(Value::as_str)
            .filter(|j| !j.is_empty() && j.len() <= 64)
            .ok_or("`jti` must be a non-empty string of at most 64 characters")?;
        Ok(json!({ "jti": jti, "newly_revoked": self.hub.revoke(jti) }))
    }
}

/// Validated `computer.hub.mint` parameters.
#[derive(Debug, PartialEq)]
pub struct MintParams {
    pub machine: Option<String>,
    pub minter: Option<String>,
    pub sub: String,
    pub roles: Vec<Role>,
    pub ttl_secs: u64,
}

fn reject_unknown(params: &Value, known: &[&str]) -> Result<(), String> {
    let obj = params.as_object().ok_or("params must be an object")?;
    match obj.keys().find(|k| !known.contains(&k.as_str())) {
        Some(k) => Err(format!("unknown parameter {k:?}")),
        None => Ok(()),
    }
}

fn opt_str(params: &Value, key: &str) -> Result<Option<String>, String> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.is_empty() && s.len() <= MAX_SUB_LEN => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{key}` must be a non-empty string of at most {MAX_SUB_LEN} characters")),
    }
}

impl MintParams {
    /// Parse and validate; a malformed parameter is refused, never defaulted.
    pub fn parse(params: &Value) -> Result<Self, String> {
        reject_unknown(params, &["machine", "minter", "sub", "roles", "ttl_secs"])?;
        let sub = opt_str(params, "sub")?.ok_or("`sub` (who the key is for) is required")?;
        let roles = match params.get("roles") {
            Some(Value::Array(a)) if !a.is_empty() && a.len() <= 2 => a
                .iter()
                .map(|r| {
                    r.as_str()
                        .and_then(Role::parse)
                        .ok_or_else(|| format!("unknown role {r}; use \"server\" or \"harness\""))
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("`roles` must be a non-empty array of \"server\" and/or \"harness\"".into()),
        };
        let ttl_secs = match params.get("ttl_secs") {
            None | Some(Value::Null) => DEFAULT_TTL_SECS,
            Some(v) => v
                .as_u64()
                .filter(|t| *t > 0)
                .ok_or("`ttl_secs` must be a positive integer")?,
        };
        Ok(Self {
            machine: opt_str(params, "machine")?,
            minter: opt_str(params, "minter")?,
            sub,
            roles,
            ttl_secs,
        })
    }
}

/// The `jti` claim of a token this module just minted.
fn decode_jti(token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: passkey::Claims = serde_json::from_slice(&bytes).ok()?;
    Some(claims.jti)
}

fn not_hosted() -> Response {
    Response::error(
        "this node does not host the computer hub (no <runtime dir>/computer-hub/hub.toml at start)"
            .to_string(),
    )
}

/// Route handler for `computer.hub.status|mint|revoke`.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        let Some(svc) = global() else { return not_hosted() };
        let out = match call.method.as_str() {
            "computer.hub.status" => Ok(svc.status()),
            "computer.hub.mint" => svc.mint(&call.params, now_secs()),
            "computer.hub.revoke" => svc.revoke(&call.params),
            other => Err(format!("unknown computer.hub method {other:?}")),
        };
        match out {
            Ok(v) => Response::success(v),
            Err(e) => Response::error(e),
        }
    })
}
