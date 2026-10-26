//! Which uid a native cog runs as, and what the broker is told about it
//! (ADR-122 gaps 2 and 3).
//!
//! A user daemon cannot switch uids; the machine service (root, or holding
//! `CAP_SETUID` and `CAP_SETGID`) can. So there are two modes, chosen per
//! placement and shown, never hidden:
//!
//! - **`per-cog-uid`**: the daemon can switch uids, `cogbus-isolation.json`
//!   names a `uid_range`, and the `cogbus` group exists. Each instance gets
//!   the lowest free uid of the range, its gid is the group's, and the
//!   `cogbus` group is its only supplementary group, so it can reach the
//!   socket (`root:cogbus 0660`) and nothing else of the daemon's. The broker
//!   registers that uid, so a connection from any other uid is refused with
//!   `uid_mismatch`.
//! - **`shared-uid`**: anything else. Cogs run as the daemon's uid (today's
//!   behaviour) and the broker's peer check proves only "a process of that
//!   user". Every such instance is marked in `cogbus list`, with the reason.
//!   `require_cog_uid` refuses a cog that uses the bus (declares `[[outputs]]`
//!   or `[[inputs]]`) in this mode; it defaults to off.
//!
//! The group, the range and any sudoers entry are created by the owner-run
//! `scripts/cogbus-uid-setup.sh`, never by the daemon.
//!
//! A uid stays with its instance across stop and start (its data directory
//! is owned by it) and is freed at unload or teardown; an identity taken by
//! a placement that failed before registering is given back at once.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use clawft_kernel::workload_ctl::host_cogbus::Isolation;
use clawft_kernel::workload_runtime::CogIdentity;
use serde::Deserialize;
use serde_json::{Value, json};

/// The operator file under the runtime dir.
pub const ISOLATION_FILE: &str = "cogbus-isolation.json";
const MAX_FILE: u64 = 4096;
/// The group that owns the socket when the file does not say otherwise.
pub const DEFAULT_GROUP: &str = "cogbus";

/// `cogbus-isolation.json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsolationConfig {
    /// Refuse a bus-using cog that would share a uid.
    #[serde(default)]
    pub require_cog_uid: bool,
    /// Inclusive `[first, last]` uid range cog uids are drawn from. It must
    /// not hold any uid that has an account or owns files.
    #[serde(default)]
    pub uid_range: Option<[u32; 2]>,
    /// Group that owns the socket and is every cog's supplementary group.
    #[serde(default = "default_group")]
    pub group: String,
}

fn default_group() -> String {
    DEFAULT_GROUP.into()
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self { require_cog_uid: false, uid_range: None, group: default_group() }
    }
}

impl IsolationConfig {
    /// Check the values a typo could make dangerous.
    pub fn validate(&self) -> Result<(), String> {
        if let Some([a, b]) = self.uid_range {
            if a == 0 || a > b || b == u32::MAX {
                return Err(format!("uid_range [{a}, {b}] must be 1 <= first <= last < 4294967295"));
            }
            if b - a >= 65_536 {
                return Err("uid_range holds more than 65536 uids".into());
            }
        }
        let ok = !self.group.is_empty()
            && self.group.len() <= 32
            && self.group.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !ok {
            return Err(format!("group {:?} is not a plain group name", self.group));
        }
        Ok(())
    }
}

/// Read `<dir>/cogbus-isolation.json`; absent is the default (shared uid).
/// A file that is group- or world-writable, too large or invalid is an error:
/// the caller runs shared and says why.
pub fn load(dir: &Path) -> Result<IsolationConfig, String> {
    use std::os::unix::fs::MetadataExt;
    let path = dir.join(ISOLATION_FILE);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(IsolationConfig::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if meta.mode() & 0o022 != 0 {
        return Err(format!("{} is group- or world-writable", path.display()));
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(format!("{} is not owned by the daemon's user", path.display()));
    }
    if meta.len() > MAX_FILE {
        return Err(format!("{} is larger than {MAX_FILE} bytes", path.display()));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let cfg: IsolationConfig = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

/// True when `CapEff` (the hex word of `/proc/self/status`) holds both
/// `CAP_SETGID` (6) and `CAP_SETUID` (7).
pub fn caps_allow_switch(cap_eff_hex: &str) -> bool {
    u64::from_str_radix(cap_eff_hex.trim(), 16).is_ok_and(|w| w & (1 << 6) != 0 && w & (1 << 7) != 0)
}

/// Whether this process can `setuid` to another user.
pub fn can_switch_uid() -> bool {
    if nix::unistd::geteuid().is_root() {
        return true;
    }
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("CapEff:").map(str::to_owned)))
        .is_some_and(|hex| caps_allow_switch(&hex))
}

/// Gid of group `name`, if it exists.
pub fn group_gid(name: &str) -> Option<u32> {
    nix::unistd::Group::from_name(name).ok().flatten().map(|g| g.gid.as_raw())
}

#[derive(Debug)]
struct Pool {
    first: u32,
    last: u32,
    /// Taken uids; `None` owner is a pending allocation.
    taken: BTreeMap<u32, Option<String>>,
}

impl Pool {
    fn allocate(&mut self) -> Option<u32> {
        let uid = (self.first..=self.last).find(|u| !self.taken.contains_key(u))?;
        self.taken.insert(uid, None);
        Some(uid)
    }
}

/// What `attach` decided for one placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The identity to run as; `None` is shared.
    pub identity: Option<CogIdentity>,
    /// Per-cog or shared.
    pub isolation: Isolation,
    /// Why not isolated.
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
struct Row {
    cog: String,
    uid: Option<u32>,
    isolation: Isolation,
}

/// The decision maker, the uid pool and the table `cogbus list` shows.
pub struct Isolator {
    cfg: IsolationConfig,
    privileged: bool,
    gid: Option<u32>,
    pool: Mutex<Option<Pool>>,
    rows: Mutex<BTreeMap<String, Row>>,
}

impl Isolator {
    /// Decide from `cfg`, the process's privilege and the group lookup.
    pub fn new(cfg: IsolationConfig, privileged: bool, gid: Option<u32>) -> Self {
        let pool = cfg.uid_range.map(|[first, last]| Pool { first, last, taken: BTreeMap::new() });
        Self { cfg, privileged, gid, pool: Mutex::new(pool), rows: Mutex::new(BTreeMap::new()) }
    }

    /// No isolation configured: every cog shares the daemon's uid.
    pub fn shared() -> Self {
        Self::new(IsolationConfig::default(), can_switch_uid(), None)
    }

    /// From the daemon's real privilege and `cfg`.
    pub fn detect(cfg: IsolationConfig) -> Self {
        let gid = group_gid(&cfg.group);
        Self::new(cfg, can_switch_uid(), gid)
    }

    /// `require_cog_uid`.
    pub fn require_cog_uid(&self) -> bool {
        self.cfg.require_cog_uid
    }

    /// Why a placement cannot get its own uid, or `None` when it can.
    pub fn blocker(&self) -> Option<String> {
        if self.cfg.uid_range.is_none() {
            return Some(format!("no uid_range in {ISOLATION_FILE}"));
        }
        if !self.privileged {
            return Some("the daemon cannot switch uids (unprivileged user daemon)".into());
        }
        if self.gid.is_none() {
            return Some(format!("group {:?} does not exist", self.cfg.group));
        }
        None
    }

    /// Allocate for one placement.
    pub fn plan(&self) -> Plan {
        let shared = |note: String| Plan { identity: None, isolation: Isolation::SharedUid, note: Some(note) };
        if let Some(why) = self.blocker() {
            return shared(why);
        }
        let (Some(gid), mut pool) = (self.gid, self.pool.lock().unwrap_or_else(|p| p.into_inner())) else {
            return shared("no gid".into());
        };
        match pool.as_mut().and_then(Pool::allocate) {
            Some(uid) => Plan {
                identity: Some(CogIdentity { uid, gid, groups: vec![gid] }),
                isolation: Isolation::PerCogUid,
                note: None,
            },
            None => shared("the uid_range is exhausted".into()),
        }
    }

    /// The instance registered with `uid`.
    pub fn bind(&self, instance: &str, cog: &str, uid: Option<u32>, isolation: Isolation) {
        if isolation == Isolation::PerCogUid
            && let (Some(uid), Some(p)) = (uid, self.pool.lock().unwrap_or_else(|p| p.into_inner()).as_mut())
        {
            p.taken.insert(uid, Some(instance.to_string()));
        }
        self.rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(instance.to_string(), Row { cog: cog.to_string(), uid, isolation });
    }

    /// The instance stopped: no longer listed, uid kept.
    pub fn unbind(&self, instance: &str) {
        self.rows.lock().unwrap_or_else(|p| p.into_inner()).remove(instance);
    }

    /// The instance is gone: free its uid.
    pub fn release(&self, instance: &str) {
        self.unbind(instance);
        if let Some(p) = self.pool.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            p.taken.retain(|_, o| o.as_deref() != Some(instance));
        }
    }

    /// A pending allocation that never registered.
    pub fn abandon(&self, uid: u32) {
        if let Some(p) = self.pool.lock().unwrap_or_else(|p| p.into_inner()).as_mut()
            && p.taken.get(&uid) == Some(&None)
        {
            p.taken.remove(&uid);
        }
    }

    /// Uids currently held (pending or bound).
    pub fn held(&self) -> BTreeSet<u32> {
        self.pool
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|p| p.taken.keys().copied().collect())
            .unwrap_or_default()
    }

    /// The `isolation` section of `cogbus.list`.
    pub fn report(&self) -> Value {
        let rows: Vec<Value> = self
            .rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, r)| {
                json!({ "instance": id, "cog": r.cog, "uid": r.uid, "isolation": r.isolation.as_str() })
            })
            .collect();
        json!({
            "mode": if self.blocker().is_none() { "per-cog-uid" } else { "shared-uid" },
            "reason": self.blocker(),
            "require_cog_uid": self.cfg.require_cog_uid,
            "group": self.cfg.group,
            "instances": rows,
        })
    }
}

#[cfg(test)]
#[path = "cogbus_isolation_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "cogbus_isolation_root_tests.rs"]
mod root_tests;
