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

use super::device_node::{NodeId, kind_matches, sensitive};
use super::device_sys::wild;
pub use super::device_node::SystemApplier;
pub use super::device_sys::{SysResolver, glob_match, globs_intersect, stat_owner};
use crate::workload_runtime::CogIdentity;
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
    /// The real path of `path`: symlinks and `..` resolved (the path itself if it does not exist).
    fn canonical(&self, path: &str) -> String {
        path.to_string()
    }
    /// What `path` is (a node's kind and identity); `None` if it does not exist.
    fn node_id(&self, _path: &str) -> Option<NodeId> {
        None
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
        /// What the node was when this was planned; the applier refuses a different one.
        expect: Option<NodeId>,
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
    ident: impl Fn(&str) -> Option<NodeId>,
) -> GrantPlan {
    let mut plan = GrantPlan::default();
    for p in paths {
        let Some((uid, gid, mode)) = stat(p) else { continue };
        let expect = ident(p);
        plan.apply.push(GrantOp::Chown {
            path: p.clone(),
            uid: id.uid,
            gid,
            mode: if write(p) { 0o600 } else { 0o400 },
            expect,
        });
        plan.restore.push(GrantOp::Chown { path: p.clone(), uid, gid, mode, expect });
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
    decls: Vec<DeviceDecl>,
    /// Real paths the selectors resolved to (symlinks followed), and `net:<name>` for interfaces.
    nodes: Vec<String>,
    /// What those nodes are: two paths with one identity are one device.
    ids: Vec<NodeId>,
    identity: Option<CogIdentity>,
    isolation: DeviceIsolation,
    /// Why the claim is `advisory`, when it is.
    note: Option<String>,
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

fn refuse(m: impl Into<String>) -> DeviceRefusal {
    DeviceRefusal(m.into())
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
    fn sels(&self) -> impl Iterator<Item = &DeviceSel> {
        self.decls.iter().map(|d| &d.sel)
    }

    /// What overlaps between two claims, named for the refusal: the same selector, selectors
    /// that can name one path, a node one selects, or the same device under another name.
    fn clash(&self, other: &Claim) -> Option<String> {
        for a in self.sels() {
            for b in other.sels() {
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
        for b in other.sels() {
            if let Some(p) = sel_pattern(b)
                && let Some(n) = self.nodes.iter().find(|n| globs_intersect(&p, n))
            {
                return Some(n.clone());
            }
        }
        if let Some(n) = self.nodes.iter().find(|n| other.nodes.contains(n)) {
            return Some(n.clone());
        }
        // The same device under two names: a /dev/v4l/by-id link and /dev/video0, a mknod copy.
        let pos = self.ids.iter().position(|i| i.is_device() && other.ids.contains(i))?;
        self.nodes.get(pos).cloned().or_else(|| Some("the same device".into()))
    }

    fn labels(&self) -> Vec<String> {
        self.sels().map(DeviceSel::label).collect()
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

    /// What the selectors name on this host now, as real paths and identities. Refuses a node
    /// that is the wrong kind for its selector (a path must be a character device, a socket
    /// selector a socket) or that no cog may own (block devices, memory, consoles, ...).
    fn resolve(&self, decls: &[DeviceDecl]) -> Result<(Vec<String>, Vec<NodeId>), DeviceRefusal> {
        let (mut nodes, mut ids) = (Vec::new(), Vec::new());
        for d in decls {
            match &d.sel {
                DeviceSel::Path(p) => self.take(self.resolver.expand(p), &d.sel, false, &mut nodes, &mut ids)?,
                DeviceSel::Socket(p) => self.take(self.resolver.expand(p), &d.sel, true, &mut nodes, &mut ids)?,
                DeviceSel::Usb { vid, pid } => {
                    self.take(self.resolver.usb_nodes(*vid, *pid), &d.sel, false, &mut nodes, &mut ids)?;
                }
                DeviceSel::Netdev(n) => nodes.extend(self.resolver.netdevs(n).into_iter().map(|x| format!("net:{x}"))),
            }
        }
        nodes.sort();
        nodes.dedup();
        ids.sort();
        ids.dedup();
        Ok((nodes, ids))
    }

    /// Add what `raw` names to `nodes` and `ids`, after the kind and sensitivity checks.
    fn take(
        &self,
        raw: Vec<String>,
        sel: &DeviceSel,
        socket: bool,
        nodes: &mut Vec<String>,
        ids: &mut Vec<NodeId>,
    ) -> Result<(), DeviceRefusal> {
        for r in raw {
            let real = self.resolver.canonical(&r);
            if let Some(id) = self.resolver.node_id(&real) {
                if let Some(why) = sensitive(&id) {
                    return Err(refuse(format!("devices: {} resolves to {real}, {why}; no cog may own it", sel.label())));
                }
                if let Err(e) = kind_matches(socket, &id)
                    && !matches!(sel, DeviceSel::Usb { .. })
                {
                    // A glob also matches directories and other kinds; only a named node must fit.
                    if sel_pattern(sel).is_some_and(|p| wild(&p)) {
                        continue;
                    }
                    return Err(refuse(format!("devices: {} resolves to {real}, but {e}", sel.label())));
                }
                ids.push(id);
            }
            nodes.push(real);
        }
        Ok(())
    }

    /// The grant plan and the isolation word for `claim`. `enforced` is only said when every owned
    /// node has a chown in the plan: an interface cannot be chowned, a node that does not exist
    /// yet has nothing to chown, and a shared uid cannot be told apart.
    fn grant(&self, claim: &mut Claim) -> GrantPlan {
        let note = match (&claim.identity, claim.sels().any(|s| matches!(s, DeviceSel::Netdev(_)))) {
            (None, _) => Some("every cog shares the daemon's uid".to_string()),
            (_, true) => Some("a network interface has no owner to change; only the ledger guards it".to_string()),
            _ if claim.nodes.is_empty() => Some("no node exists yet to give to the owner".to_string()),
            _ => None,
        };
        let mut plan = GrantPlan::default();
        if let (Some(id), None) = (claim.identity.clone(), &note) {
            let decls = claim.decls.clone();
            let writable = |p: &str| {
                let named: Vec<&DeviceDecl> =
                    decls.iter().filter(|d| sel_pattern(&d.sel).is_some_and(|g| glob_match(&g, p))).collect();
                if named.is_empty() {
                    decls.iter().any(|d| d.write && matches!(d.sel, DeviceSel::Usb { .. }))
                } else {
                    named.iter().any(|d| d.write)
                }
            };
            plan = plan_grants(&claim.nodes, &id, writable, |p| self.resolver.stat(p), |p| self.resolver.node_id(p));
        }
        let all = plan.apply.len() == claim.nodes.len();
        claim.isolation = if note.is_none() && all { DeviceIsolation::Enforced } else { DeviceIsolation::Advisory };
        claim.note = note.or_else(|| (!all).then(|| "some owned nodes could not be given to the owner".to_string()));
        claim.restore.clone_from(&plan.restore);
        plan
    }

    /// Whether `cand` (an argument, or a piece of one) names a device `other` owns that `claim`
    /// does not: by glob, by real path, or by identity (another name for the same device).
    fn names_foreign(&self, claim: &Claim, other: &Claim, cand: &str) -> bool {
        let owns = |c: &Claim, p: &str| {
            c.sels().any(|s| sel_pattern(s).is_some_and(|g| glob_match(&g, p))) || c.nodes.iter().any(|n| n == p)
        };
        if !cand.starts_with('/') {
            // An interface name, bare or after `=`.
            return other.sels().any(|s| matches!(s, DeviceSel::Netdev(n) if glob_match(n, cand)))
                && !claim.sels().any(|s| matches!(s, DeviceSel::Netdev(n) if glob_match(n, cand)));
        }
        let real = self.resolver.canonical(cand);
        let id = self.resolver.node_id(&real).filter(NodeId::is_device);
        let theirs = owns(other, cand) || owns(other, &real) || id.is_some_and(|i| other.ids.contains(&i));
        let ours = owns(claim, cand) || owns(claim, &real) || id.is_some_and(|i| claim.ids.contains(&i));
        theirs && !ours
    }

    /// Admit a placement of `cog` with `decls` and `args`.
    ///
    /// Refuses when a declared device overlaps another instance's claim (under any name), is a
    /// kind or a device no cog may own, or when an argument names a device another instance owns
    /// (`--device /dev/video0`, `--device=...`, `-d/dev/video0`, `--src=a:/dev/video0`, a
    /// symlink to it). `Ok(None)`: the cog declares no device and its arguments are clean. The
    /// reservation holds the claim until [`DeviceReservation::bind`] gives it to an instance;
    /// dropped unbound, it is released.
    pub fn admit(
        self: &Arc<Self>,
        cog: &str,
        decls: &[DeviceDecl],
        args: &[String],
        identity: Option<&CogIdentity>,
    ) -> Result<Option<DeviceReservation>, DeviceRefusal> {
        let (nodes, ids) = self.resolve(decls)?;
        let mut claim = Claim {
            instance: String::new(),
            cog: cog.to_string(),
            decls: decls.to_vec(),
            nodes,
            ids,
            identity: identity.cloned(),
            isolation: DeviceIsolation::Advisory,
            note: None,
            restore: Vec::new(),
        };
        let mut g = self.lock();
        let cands = arg_candidates(args);
        for other in g.claims.values() {
            let who = if other.instance.is_empty() { "being placed" } else { &other.instance };
            if let Some(dev) = claim.clash(other) {
                return Err(refuse(format!("devices: {dev} is owned by {} ({who}); one owner per device", other.cog)));
            }
            if let Some(c) = cands.iter().find(|c| self.names_foreign(&claim, other, c)) {
                return Err(refuse(format!(
                    "devices: argument {c} names a device owned by {} ({who}); only its owner may open it",
                    other.cog
                )));
            }
        }
        if decls.is_empty() {
            return Ok(None);
        }
        let plan = self.grant(&mut claim);
        g.next += 1;
        let key = format!("pending-{}", g.next);
        let paths = granted_paths(&claim);
        g.claims.insert(key.clone(), claim);
        drop(g);
        Ok(Some(DeviceReservation { ledger: self.clone(), key, bound: false, paths, plan }))
    }

    /// The owner is about to (re)start: resolve its selectors again, because a replugged USB
    /// device has new node numbers and a glob may now match more. Refuses if a node it now names
    /// belongs to another owner or is one no cog may own; otherwise returns the paths to hand
    /// out and the grants for nodes that were not there before (for the applier).
    pub fn refresh(&self, instance: &str) -> Result<(Vec<String>, Vec<GrantOp>), DeviceRefusal> {
        let (key, decls) = {
            let g = self.lock();
            let Some((k, c)) = g.claims.iter().find(|(_, c)| c.instance == instance) else {
                return Ok((Vec::new(), Vec::new()));
            };
            (k.clone(), c.decls.clone())
        };
        let (nodes, ids) = self.resolve(&decls)?;
        let mut g = self.lock();
        let mut mine = g.claims.get(&key).cloned().ok_or_else(|| refuse("the claim is gone"))?;
        let before = mine.nodes.clone();
        mine.nodes = nodes;
        mine.ids = ids;
        for (k, other) in &g.claims {
            if *k != key
                && let Some(dev) = mine.clash(other)
            {
                return Err(refuse(format!("devices: {dev} now belongs to {}; {instance} may not start on it", other.cog)));
            }
        }
        let old_restore = mine.restore.clone();
        let plan = self.grant(&mut mine);
        let fresh: Vec<GrantOp> = plan.apply.iter().filter(|GrantOp::Chown { path, .. }| !before.contains(path)).cloned().collect();
        let extra: Vec<GrantOp> = old_restore.into_iter().filter(|r| !mine.restore.contains(r)).collect();
        mine.restore.extend(extra);
        let paths = granted_paths(&mine);
        g.claims.insert(key, mine);
        Ok((paths, fresh))
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
                    "device_isolation": c.isolation.as_str(), "isolation_note": c.note,
                })
            })
            .collect();
        json!({ "owners": rows })
    }
}

/// Every piece of an argument that could name a device: the argument, what follows `=`, and
/// each suffix that starts at a `/` (so `-d/dev/video0`, `--src=a:/dev/video0` and
/// `--opt=/dev/video0,/dev/video1` all yield paths), cut at `,`, `:`, `;` or a space as well.
fn arg_candidates(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for a in args {
        out.push(a.clone());
        if let Some((_, v)) = a.split_once('=') {
            out.push(v.to_string());
        }
        for (i, _) in a.match_indices('/') {
            let rest = &a[i..];
            out.push(rest.to_string());
            if let Some(end) = rest.find([',', ':', ';', ' ']) {
                out.push(rest[..end].to_string());
            }
        }
    }
    out.retain(|c| !c.is_empty());
    out.sort();
    out.dedup();
    out
}

/// What the owner is handed: the real paths its selectors resolved to, the literal paths it
/// declared, and interface names (plain, no leading slash).
fn granted_paths(c: &Claim) -> Vec<String> {
    let mut out: Vec<String> = c.sels().filter_map(sel_pattern).filter(|p| !wild(p)).collect();
    out.extend(c.nodes.iter().cloned());
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
