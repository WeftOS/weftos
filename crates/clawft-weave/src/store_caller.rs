//! Verified caller identity for the `store.*` verbs (ADR-126 S2, card 6f0bc9c8).
//!
//! The `store.worktree.*` verbs and `store grant` run on the daemon's operator path, and the
//! principal on them (`WEFTOS_AGENT`) is a label the caller supplies. On a daemon that serves
//! several principals that lets a read-only principal publish, claim or grant itself write
//! (drill findings D1-1 and D1-2).
//!
//! The fix derives the caller from the socket peer's uid, which the daemon reads at accept
//! (`SO_PEERCRED` / `LOCAL_PEERCRED`, through tokio; never from the request), and maps it to a
//! store principal through a daemon-side file, `store-principals.toml` in the runtime directory:
//!
//! ```toml
//! [uids]
//! 1001 = "agent:follower"
//! 1002 = "agent:writer"
//! ```
//!
//! The mode is off unless the operator switches it on (`WEFTOS_STORE_PRINCIPALS=1` in the daemon's
//! environment). Off, the daemon behaves as before: one daemon is one trust domain and every caller
//! that passes the Admin capability check is the operator, and a map file that happens to exist is
//! ignored (with a warning). On, the file must exist, be owned by the daemon's uid and not be
//! writable by group or other, or the store does not start and every `store.*` verb is refused
//! except to the operator: a missing or unusable file never means open mode. With the mode on:
//! - the daemon's own uid is the operator (its map entry, if any, is a configuration error);
//! - a mapped uid is that principal, and each verb is checked against the principal's grants;
//! - an unmapped uid, a supervised child and a peer with no readable uid are refused.
//!
//! A uid is the only thing the daemon can verify about a same-machine peer, so two principals
//! must run as two uids. A principal must send `agent` equal to its full principal (`agent:name`,
//! kind and id), so the registry, chain and claim records name the verified identity; an absent or
//! different label is refused.
//!
//! The verbs that read or write a path the caller names (`publish`, `fetch`, `follow`,
//! `sync-docs`) run those paths as the daemon's uid, so a principal under another uid could use the
//! daemon to reach files it cannot. They are the operator's alone. Moving that file I/O to the
//! client is a follow-up.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use clawft_rpc::Response;
use serde_json::Value;
use weftos_store::{Access, Principal, StoreName, StoreService};

use crate::rpc_ext::CallerCtx;

/// File in the runtime directory that maps socket peer uids to store principals.
pub const MAP_FILE: &str = "store-principals.toml";

/// Error kind of a refusal.
pub const REFUSED_KIND: &str = "store_caller_refused";

/// uid to store principal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrincipalMap {
    by_uid: HashMap<u32, Principal>,
}

impl PrincipalMap {
    /// Parse the file text. `own_uid` is the daemon's uid: it is the operator and cannot be mapped.
    pub fn parse(text: &str, own_uid: u32) -> Result<Self, String> {
        let v: toml::Value = toml::from_str(text).map_err(|e| format!("{MAP_FILE}: {e}"))?;
        let mut by_uid = HashMap::new();
        let Some(uids) = v.get("uids") else { return Ok(Self { by_uid }) };
        let t = uids.as_table().ok_or_else(|| format!("{MAP_FILE}: [uids] must be a table"))?;
        for (k, val) in t {
            let uid: u32 = k.parse().map_err(|_| format!("{MAP_FILE}: uid {k:?} is not a number"))?;
            if uid == own_uid {
                return Err(format!("{MAP_FILE}: uid {uid} is the daemon's own uid, which is the operator; run a principal under its own uid"));
            }
            let s = val.as_str().ok_or_else(|| format!("{MAP_FILE}: uid {uid} must map to a string like \"agent:name\""))?;
            let p = Principal::parse(s).map_err(|e| format!("{MAP_FILE}: uid {uid}: {e}"))?;
            by_uid.insert(uid, p);
        }
        Ok(Self { by_uid })
    }

    /// Read `dir/store-principals.toml`. `Ok(None)` when the file does not exist. The file must be a
    /// regular file owned by `own_uid` that group and other cannot write: the daemon trusts its
    /// content to decide who may write the store. The checks run on the opened file, so they hold
    /// for the bytes that are parsed.
    pub fn load(dir: &Path, own_uid: u32) -> Result<Option<Self>, String> {
        use std::io::Read;
        let mut f = match std::fs::File::open(dir.join(MAP_FILE)) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{MAP_FILE}: {e}")),
        };
        let md = f.metadata().map_err(|e| format!("{MAP_FILE}: {e}"))?;
        if !md.is_file() {
            return Err(format!("{MAP_FILE}: not a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if md.uid() != own_uid {
                return Err(format!("{MAP_FILE}: owned by uid {}, not the daemon's uid {own_uid}", md.uid()));
            }
            if md.mode() & 0o022 != 0 {
                return Err(format!("{MAP_FILE}: writable by group or other (mode {:o}); chmod go-w it", md.mode() & 0o7777));
            }
        }
        let mut t = String::new();
        f.read_to_string(&mut t).map_err(|e| format!("{MAP_FILE}: {e}"))?;
        Self::parse(&t, own_uid).map(Some)
    }

    /// How many uids are mapped.
    pub fn len(&self) -> usize {
        self.by_uid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_uid.is_empty()
    }

    pub fn get(&self, uid: u32) -> Option<&Principal> {
        self.by_uid.get(&uid)
    }
}

/// The verified caller of a `store.*` verb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreCaller {
    /// The daemon's own uid, or any caller when no map is configured: operator rights.
    Operator,
    /// A mapped uid: bound by this principal's grants.
    Principal(Principal),
}

/// Serialises the tests that install the process-wide map.
#[cfg(test)]
pub(crate) static TEST_MAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

static MAP: RwLock<Option<Arc<PrincipalMap>>> = RwLock::new(None);

/// Install (or clear, with `None`) the daemon's map. Called once at store start.
pub fn install(map: Option<PrincipalMap>) {
    if let Ok(mut g) = MAP.write() {
        *g = map.map(Arc::new);
    }
}

/// Environment switch for principal mode. The map file alone does not turn it on.
pub const ENABLE_ENV: &str = "WEFTOS_STORE_PRINCIPALS";

/// Is `v`, the switch's value, an "on"?
pub fn enabled_value(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
}

/// Is principal mode switched on in this process's environment?
pub fn enabled_from_env() -> bool {
    enabled_value(std::env::var(ENABLE_ENV).ok().as_deref())
}

/// Set up principal mode at store start. Off: nothing is installed (a map file that exists is
/// ignored, with a warning). On: the file is required and vetted; when it is missing or unusable
/// the deny-all-but-the-operator map is installed and an error returned, so the store does not run
/// and the gate cannot fall back to open mode.
pub fn configure(dir: &Path, own_uid: u32, enabled: bool) -> Result<(), String> {
    if !enabled {
        if dir.join(MAP_FILE).exists() {
            tracing::warn!("{} exists but {ENABLE_ENV} is not on: store principal mode is OFF and every caller that passes the Admin check is the operator", dir.join(MAP_FILE).display());
        }
        install(None);
        return Ok(());
    }
    match PrincipalMap::load(dir, own_uid) {
        Ok(Some(m)) => {
            tracing::info!(mapped_uids = m.len(), "store principal mode ON: callers are identified by socket peer uid ({MAP_FILE})");
            install(Some(m));
            Ok(())
        }
        Ok(None) => {
            install(Some(PrincipalMap::default()));
            Err(format!("{ENABLE_ENV} is on but {} does not exist; refusing to run the store in open mode", dir.join(MAP_FILE).display()))
        }
        Err(e) => {
            install(Some(PrincipalMap::default()));
            Err(format!("{ENABLE_ENV} is on and {e}; refusing to run the store in open mode"))
        }
    }
}

fn installed() -> Option<Arc<PrincipalMap>> {
    MAP.read().ok().and_then(|g| g.clone())
}

fn own_uid() -> u32 {
    #[cfg(unix)]
    {
        nix::unistd::geteuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        u32::MAX
    }
}

/// Who is calling, from the peer identity the daemon stamped on the connection.
pub fn identify(map: Option<&PrincipalMap>, caller: &CallerCtx, own_uid: u32) -> Result<StoreCaller, String> {
    let Some(map) = map else { return Ok(StoreCaller::Operator) };
    if caller.peer_child {
        return Err("a supervised project kernel may not call store verbs".into());
    }
    let uid = caller.peer_key.as_deref().and_then(crate::child_peer::uid_of_peer_key);
    match uid {
        None => Err("the daemon cannot verify who is calling (no socket peer uid)".into()),
        Some(u) if u == own_uid && !caller.peer_untrusted => Ok(StoreCaller::Operator),
        Some(u) => map.get(u).cloned().map(StoreCaller::Principal).ok_or_else(|| format!("uid {u} is not mapped to a store principal in {MAP_FILE}")),
    }
}

/// Is `method` one the gate covers?
pub fn is_store_method(method: &str) -> bool {
    method.starts_with("store.")
}

/// Request fields the worktree verbs open as host paths. A principal's call carrying one is refused
/// whatever the verb, so a verb added later cannot reopen the hole.
const HOST_PATH_FIELDS: [&str; 9] = ["root", "dest", "cache", "state", "bundle", "bundle_out", "paths_z", "uncommitted_z", "changes_z"];

fn need(svc: &StoreService, who: &Principal, name: &StoreName, access: Access) -> Result<(), String> {
    if svc.holds_at_least(who, name, access) {
        Ok(())
    } else {
        Err(format!("{who} does not hold {} on {name}", access.as_str()))
    }
}

fn s<'a>(params: &'a Value, k: &str) -> Result<&'a str, String> {
    params[k].as_str().filter(|v| !v.is_empty()).ok_or_else(|| format!("missing {k}"))
}

fn store_name(params: &Value) -> Result<StoreName, String> {
    StoreName::parse(s(params, "name")?).map_err(|e| e.to_string())
}

/// Check one verb against a verified caller. The operator passes; a principal passes only what its
/// grants allow, and anything this function does not list is the operator's alone.
pub fn check(svc: &StoreService, mesh: &str, who: &StoreCaller, method: &str, params: &Value) -> Result<(), String> {
    let StoreCaller::Principal(p) = who else { return Ok(()) };
    let e = |e: weftos_store::StoreError| e.to_string();
    if let Some(verb) = method.strip_prefix("store.worktree.") {
        // The label names the actor in the registry, the chain and the claim records. It must be the
        // verified principal in full (kind and id): absent, bare or another kind is refused.
        match params["agent"].as_str().filter(|a| !a.is_empty()) {
            Some(a) if a == p.to_string() => {}
            Some(a) => return Err(format!("agent label {a:?} is not the verified principal {p}")),
            None => return Err(format!("agent label required: send {p}")),
        }
        // H1: a path the caller names is opened by the daemon as its own uid. A principal runs under
        // another uid, so it must not reach the daemon's files through these verbs.
        if let Some(f) = HOST_PATH_FIELDS.iter().find(|f| params.get(**f).is_some_and(|v| !v.is_null())) {
            return Err(format!("store.worktree.{verb} with {f}: a path on the daemon's host is for the operator"));
        }
        let project = s(params, "project")?;
        let pr = StoreName::project(mesh, project).map_err(e)?;
        let wt = |slug: Result<&str, String>| -> Result<StoreName, String> { StoreName::worktree(mesh, project, slug?).map_err(e) };
        return match verb {
            "create" => need(svc, p, &pr, Access::Write),
            "claim" | "orphan" => need(svc, p, &wt(s(params, "slug"))?, Access::Write),
            // `promote` rewrites the worktree's handoff and landed state, so it needs write on both.
            "promote" => need(svc, p, &pr, Access::Write).and_then(|()| need(svc, p, &wt(s(params, "slug"))?, Access::Write)),
            "buckets" | "usage" | "search" | "leftovers" | "browse" | "overlaps" => need(svc, p, &pr, Access::Read),
            "publish" | "fetch" | "follow" | "sync-docs" => Err(format!("store.worktree.{verb} reads or writes a path on the daemon's host as the daemon's uid: it is for the operator")),
            // `gc` sweeps every worktree of a project, so it is the operator's.
            other => Err(format!("store.worktree.{other} is for the operator")),
        };
    }
    match method {
        "store.grant" | "store.revoke" => need(svc, p, &store_name(params)?, Access::Admin),
        // These take a principal parameter; it must be the verified one, and the store's own grant check follows.
        "store.put" | "store.get" => {
            let claimed = s(params, "principal")?;
            if claimed != p.to_string() {
                return Err(format!("principal {claimed:?} is not the verified principal {p}"));
            }
            Ok(())
        }
        other => Err(format!("{other} is for the operator")),
    }
}

/// The daemon's hook, called from the one authorization point. `None`: not applicable (not a store
/// verb, no map configured, or the operator), so the ordinary checks decide. `Some(Ok)`: a mapped
/// principal whose grants allow this verb; the Admin capability check does not apply to it.
/// `Some(Err)`: refused.
pub fn gate(caller: &CallerCtx, method: &str, params: &Value) -> Option<Result<(), Response>> {
    if !is_store_method(method) {
        return None;
    }
    let map = installed()?;
    let refuse = |why: String| Some(Err(Response::error_with_kind(REFUSED_KIND, format!("{method}: {why}"))));
    let who = match identify(Some(&map), caller, own_uid()) {
        Ok(StoreCaller::Operator) => return None,
        Ok(w) => w,
        Err(why) => return refuse(why),
    };
    let Some(svc) = crate::store_rt::global() else { return refuse("the object store is not running on this daemon".into()) };
    match check(&svc, &crate::store_rt::mesh_token(), &who, method, params) {
        Ok(()) => Some(Ok(())),
        Err(why) => refuse(why),
    }
}

#[cfg(test)]
#[path = "store_caller_tests.rs"]
mod tests;
