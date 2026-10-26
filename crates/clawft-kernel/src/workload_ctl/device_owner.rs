//! One owner per device (Ventuno cog plan 4.1, 4.7; ADR-122 section 4b).
//!
//! A cog lists the devices it opens as `[[devices]]` in `cog.toml`. The node's
//! [`DeviceLedger`] records the owner of each and refuses:
//!
//! - **a second claim.** A placement whose `[[devices]]` overlap an existing
//!   claim on this node (two globs that can name the same node count) is
//!   refused at admission, before anything is loaded;
//! - **a hand-out to a non-owner.** The device list in the instance's
//!   environment (`WEFTOS_DEVICES`) comes only from the owner's reservation,
//!   and a place whose arguments name a device another instance owns
//!   (`--device /dev/video0`) is refused.
//!
//! What the node can enforce depends on how cogs are run, and the listing says
//! which, never implies it:
//!
//! - `enforced`: the cog runs under its own uid (per-cog-uid, ADR-122 4a) and
//!   the ledger plans a `chown`/`chmod` of the owned nodes to that uid
//!   ([`GrantPlan`]), applied before start and undone at unload. A second cog
//!   with another uid cannot open the node even if its code tries.
//! - `advisory`: every cog shares the daemon's uid (a user daemon). File
//!   permissions cannot tell cogs apart, so the ledger and the checks above
//!   are the whole guard; a cog that opens a device node it never declared is
//!   not stopped.
//!
//! The plan and the checks are data and pure functions, so they are tested
//! without a device or root; [`DeviceApplier`] is the only part that touches
//! the host.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::workload_runtime::CogIdentity;
use super::device_sys::wild;
pub use super::device_sys::{SysResolver, SystemApplier, glob_match, globs_intersect, stat_owner};
use crate::workload_runtime::cog_devices::{DeviceDecl, DeviceSel};

/// Whether the node can keep a non-owner away from the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeviceIsolation {
    /// The owner runs under its own uid and the nodes are chowned to it.
    Enforced,
    /// A shared uid: the ledger is the only guard.
    Advisory,
}

impl DeviceIsolation {
    /// The word shown in listings.
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceIsolation::Enforced => "enforced",
            DeviceIsolation::Advisory => "advisory",
        }
    }
}

/// Where the ledger looks up what exists on this node.
pub trait DeviceResolver: Send + Sync {
    /// Existing nodes matching `pattern` (a path, or a glob in its last
    /// component), sorted.
    fn expand(&self, pattern: &str) -> Vec<String>;
    /// Device nodes that belong to USB device `vid:pid`.
    fn usb_nodes(&self, vid: u16, pid: u16) -> Vec<String>;
    /// `(uid, gid, mode)` of `path`; `None` if it does not exist.
    fn stat(&self, path: &str) -> Option<(u32, u32, u32)>;
    /// Existing network interfaces matching `pattern`, sorted.
    fn netdevs(&self, _pattern: &str) -> Vec<String> {
        Vec::new()
    }
}

/// One step of a privileged grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantOp {
    /// Make `uid:gid` the owner of `path` and set its mode.
    Chown {
        /// Device node or socket.
        path: String,
        /// New owner.
        uid: u32,
        /// Group (the node's own, kept).
        gid: u32,
        /// New permission bits.
        mode: u32,
    },
}

/// What to do to the host for one owner, and how to put it back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantPlan {
    /// Before start.
    pub apply: Vec<GrantOp>,
    /// At unload: the owner, group and mode each node had.
    pub restore: Vec<GrantOp>,
}

/// Plan the grant of `paths` to `id`. `stat` returns `(uid, gid, mode)`; a
/// path it cannot stat is skipped (it does not exist yet).
pub fn plan_grants(
    paths: &[String],
    id: &CogIdentity,
    write: impl Fn(&str) -> bool,
    stat: impl Fn(&str) -> Option<(u32, u32, u32)>,
) -> GrantPlan {
    let mut plan = GrantPlan::default();
    for p in paths {
        let Some((uid, gid, mode)) = stat(p) else { continue };
        plan.apply.push(GrantOp::Chown {
            path: p.clone(),
            uid: id.uid,
            gid,
            mode: if write(p) { 0o600 } else { 0o400 },
        });
        plan.restore.push(GrantOp::Chown { path: p.clone(), uid, gid, mode });
    }
    plan
}

/// Carries a plan out on the host.
pub trait DeviceApplier: Send + Sync {
    /// Run `ops` in order, stopping at the first error.
    fn apply(&self, ops: &[GrantOp]) -> Result<(), String>;
}

#[derive(Debug, Clone)]
struct Claim {
    instance: String,
    cog: String,
    sels: Vec<DeviceSel>,
    /// Existing nodes the selectors resolved to when the claim was made.
    nodes: Vec<String>,
    isolation: DeviceIsolation,
    restore: Vec<GrantOp>,
}

/// Why a placement may not have its devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRefusal(pub String);

impl std::fmt::Display for DeviceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The glob a selector claims, in its own namespace: paths start with `/`, interfaces with
/// `net:`, so a `*` interface never overlaps a path.
fn sel_pattern(s: &DeviceSel) -> Option<String> {
    match s {
        DeviceSel::Path(p) | DeviceSel::Socket(p) => Some(p.clone()),
        DeviceSel::Netdev(n) => Some(format!("net:{n}")),
        DeviceSel::Usb { .. } => None,
    }
}

impl Claim {
    /// What overlaps between two claims, named for the refusal.
    fn clash(&self, other: &Claim) -> Option<String> {
        for a in &self.sels {
            for b in &other.sels {
                let hit = match (sel_pattern(a), sel_pattern(b)) {
                    (Some(x), Some(y)) => globs_intersect(&x, &y),
                    _ => a == b,
                };
                if hit {
                    return Some(if a == b { a.label() } else { format!("{} and {}", a.label(), b.label()) });
                }
            }
            if let Some(p) = sel_pattern(a)
                && let Some(n) = other.nodes.iter().find(|n| globs_intersect(&p, n))
            {
                return Some(n.clone());
            }
        }
        for b in &other.sels {
            if let Some(p) = sel_pattern(b)
                && let Some(n) = self.nodes.iter().find(|n| globs_intersect(&p, n))
            {
                return Some(n.clone());
            }
        }
        self.nodes.iter().find(|n| other.nodes.contains(n)).cloned()
    }

    /// True if `path` is one of the devices this claim owns.
    fn owns(&self, path: &str) -> bool {
        self.nodes.iter().any(|n| n == path) || self.sels.iter().any(|s| sel_pattern(s).is_some_and(|p| glob_match(&p, path)))
    }

    fn labels(&self) -> Vec<String> {
        self.sels.iter().map(DeviceSel::label).collect()
    }
}

#[derive(Default)]
struct Inner {
    claims: BTreeMap<String, Claim>,
    next: u64,
}

/// The node's record of who owns which device.
pub struct DeviceLedger {
    inner: Mutex<Inner>,
    resolver: Box<dyn DeviceResolver>,
}

impl DeviceLedger {
    /// A ledger resolving against `resolver`.
    pub fn new(resolver: Box<dyn DeviceResolver>) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(Inner::default()), resolver })
    }

    /// A ledger on this host.
    pub fn system() -> Arc<Self> {
        Self::new(Box::new(SysResolver))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Admit a placement of `cog` with `decls` and `args`.
    ///
    /// Refuses when a declared device overlaps another instance's claim, or
    /// when an argument names a device another instance owns. `Ok(None)`: the
    /// cog declares no device and its arguments are clean. The reservation
    /// holds the claim until [`DeviceReservation::bind`] gives it to an
    /// instance; dropped unbound, it is released.
    pub fn admit(
        self: &Arc<Self>,
        cog: &str,
        decls: &[DeviceDecl],
        args: &[String],
        identity: Option<&CogIdentity>,
    ) -> Result<Option<DeviceReservation>, DeviceRefusal> {
        let sels: Vec<DeviceSel> = decls.iter().map(|d| d.sel.clone()).collect();
        let mut nodes = Vec::new();
        for s in &sels {
            match s {
                DeviceSel::Path(p) | DeviceSel::Socket(p) => nodes.extend(self.resolver.expand(p)),
                DeviceSel::Netdev(n) => nodes.extend(self.resolver.netdevs(n).into_iter().map(|d| format!("net:{d}"))),
                DeviceSel::Usb { vid, pid } => nodes.extend(self.resolver.usb_nodes(*vid, *pid)),
            }
        }
        nodes.sort();
        nodes.dedup();
        let isolation = if identity.is_some() { DeviceIsolation::Enforced } else { DeviceIsolation::Advisory };
        let mut claim = Claim {
            instance: String::new(),
            cog: cog.to_string(),
            sels,
            nodes,
            isolation,
            restore: Vec::new(),
        };
        let mut g = self.lock();
        for other in g.claims.values() {
            let who = if other.instance.is_empty() { "being placed" } else { &other.instance };
            if let Some(dev) = claim.clash(other) {
                return Err(DeviceRefusal(format!(
                    "devices: {dev} is owned by {} ({who}); one owner per device",
                    other.cog
                )));
            }
            if let Some(path) = args_naming(args).find(|a| other.owns(a) && !claim.owns(a)) {
                return Err(DeviceRefusal(format!(
                    "devices: argument {path} names a device owned by {} ({who}); only its owner may open it",
                    other.cog
                )));
            }
        }
        if decls.is_empty() {
            return Ok(None);
        }
        let writable = |p: &str| {
            let named: Vec<&DeviceDecl> =
                decls.iter().filter(|d| sel_pattern(&d.sel).is_some_and(|g| glob_match(&g, p))).collect();
            if named.is_empty() {
                decls.iter().any(|d| d.write && matches!(d.sel, DeviceSel::Usb { .. }))
            } else {
                named.iter().any(|d| d.write)
            }
        };
        let mut grant = GrantPlan::default();
        if let Some(id) = identity {
            grant = plan_grants(&claim.nodes, id, writable, |p| self.resolver.stat(p));
            claim.restore.clone_from(&grant.restore);
        }
        g.next += 1;
        let key = format!("pending-{}", g.next);
        let paths = granted_paths(&claim);
        g.claims.insert(key.clone(), claim);
        drop(g);
        Ok(Some(DeviceReservation { ledger: self.clone(), key, bound: false, paths, plan: grant }))
    }

    /// Give up the claim of `instance`; returns the plan that restores the
    /// nodes' owners, for the caller to apply.
    pub fn release(&self, instance: &str) -> Vec<GrantOp> {
        let mut g = self.lock();
        let keys: Vec<String> = g.claims.iter().filter(|(_, c)| c.instance == instance).map(|(k, _)| k.clone()).collect();
        keys.iter().filter_map(|k| g.claims.remove(k)).flat_map(|c| c.restore).collect()
    }

    /// `(devices, isolation)` of `instance`'s claim.
    pub fn claim_of(&self, instance: &str) -> Option<(Vec<String>, DeviceIsolation)> {
        self.lock().claims.values().find(|c| c.instance == instance).map(|c| (c.labels(), c.isolation))
    }

    /// Every claim, for `cogbus list` and the status of a node.
    pub fn report(&self) -> Value {
        let g = self.lock();
        let rows: Vec<Value> = g
            .claims
            .values()
            .filter(|c| !c.instance.is_empty())
            .map(|c| {
                json!({
                    "instance": c.instance, "cog": c.cog, "devices": c.labels(), "nodes": c.nodes,
                    "device_isolation": c.isolation.as_str(),
                })
            })
            .collect();
        json!({ "owners": rows })
    }
}

/// Absolute-path values in `args`, with `--flag=/path` split.
fn args_naming(args: &[String]) -> impl Iterator<Item = &str> {
    args.iter().map(|a| a.split_once('=').map_or(a.as_str(), |(_, v)| v)).filter(|v| v.starts_with('/'))
}

/// What the owner is handed: literal paths and the nodes selectors resolved to.
fn granted_paths(c: &Claim) -> Vec<String> {
    let mut out: Vec<String> = c.sels.iter().filter_map(sel_pattern).filter(|p| !wild(p)).collect();
    out.extend(c.nodes.iter().cloned());
    // Interfaces are handed by their plain names; paths keep their leading `/`.
    for p in &mut out {
        if let Some(n) = p.strip_prefix("net:") {
            *p = n.to_string();
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A claim not yet bound to an instance.
pub struct DeviceReservation {
    ledger: Arc<DeviceLedger>,
    key: String,
    bound: bool,
    paths: Vec<String>,
    plan: GrantPlan,
}

impl DeviceReservation {
    /// The device paths the owner is handed (`WEFTOS_DEVICES`).
    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// The host changes to apply before the owner starts (empty when shared).
    pub fn plan(&self) -> &GrantPlan {
        &self.plan
    }

    /// The loaded instance is the owner.
    pub fn bind(mut self, instance: &str) {
        if let Some(c) = self.ledger.lock().claims.get_mut(&self.key) {
            c.instance = instance.to_string();
        }
        self.bound = true;
    }
}

impl Drop for DeviceReservation {
    fn drop(&mut self) {
        if !self.bound {
            self.ledger.lock().claims.remove(&self.key);
        }
    }
}
