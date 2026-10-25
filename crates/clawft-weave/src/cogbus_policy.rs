//! Governance for this node's cogbus (ADR-122 section 5, phase B3): the
//! broker's [`BusPolicy`], over the kernel's governance rules and the
//! operator's `cogbus-policy.json`.
//!
//! The broker's manifest rules run first; this can only refuse more. With
//! no file and no matching rule everything the manifest allows is allowed,
//! and no other project's topic is granted.
//!
//! - **Kernel rules.** Each request is evaluated as a `GovernanceRequest`:
//!   agent the instance id, action `cogbus.declare`, `cogbus.publish` or
//!   `cogbus.subscribe`, context `tool` = the topic (so a rule's
//!   `tool_selector` can name one, e.g. `telemetry/*`), `cog_id` and
//!   `schema`; project attribution is the kernel's own
//!   ([`crate::cogbus_governance`]). The rules are the project
//!   kernel's effective set (parent policy plus overlay, so an overlay deny
//!   on `cogbus.*` applies) or the user daemon's gate rules, as of daemon
//!   start. Decisions are not chained (ADR-122: no chain event per
//!   subscription); the activity log records them.
//! - **`<runtime dir>/cogbus-policy.json`**: owner-only like every
//!   `workload-*.json` policy file (not group- or world-writable, owned by
//!   the daemon's user), size-capped, re-read when it changes. A file that
//!   does not parse or validate refuses every request until fixed.
//!
//! ```json
//! {
//!   "grants": [
//!     {"to": "<reader project>", "from": "<topic project>",
//!      "topics": ["telemetry/*", "detections/cam-0"], "cogs": ["dashboard-feed"]}
//!   ],
//!   "deny": [
//!     {"actions": ["cogbus.subscribe"], "topics": ["audio.pcm/*"],
//!      "cogs": ["untrusted"], "projects": ["<project>"], "reason": "no audio"}
//!   ]
//! }
//! ```
//!
//! A grant lets cogs of project `to` (only the listed `cogs`, when given)
//! subscribe to `from`'s topics matching `topics`; a `<kind>/*` wildcard
//! subscribe then also sees those topics. The node's own bucket (cogs with
//! no project) can neither grant nor be granted. A deny matches when every
//! field it sets matches; `topics` entries are `<kind>/<source>`,
//! `<kind>/*` or `*`, and a `<kind>/*` subscribe is refused when a deny
//! names any source of that kind (the wildcard would read it).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use clawft_kernel::governance::{GovernanceEngine, GovernanceRule};

use crate::cogbus_governance::BusAsk;
use serde::Deserialize;
use weftos_cogbus::{BusAction, BusPolicy, BusRequest, Grant};

/// The operator file under the runtime dir.
pub const POLICY_FILE: &str = "cogbus-policy.json";
const MAX_ENTRIES: usize = 256;
const ACTIONS: [&str; 3] = ["cogbus.declare", "cogbus.publish", "cogbus.subscribe"];

/// One cross-project grant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantEntry {
    /// Project whose cogs may read.
    pub to: String,
    /// Project whose topics they read.
    pub from: String,
    /// `<kind>/<source>`, `<kind>/*` or `*`.
    pub topics: Vec<String>,
    /// Only these cogs of `to`; empty is every cog.
    #[serde(default)]
    pub cogs: Vec<String>,
}

/// One deny rule; every field it sets must match.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenyEntry {
    /// `cogbus.declare` / `.publish` / `.subscribe`; empty is all three.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Topic patterns; empty is every topic.
    #[serde(default)]
    pub topics: Vec<String>,
    /// Cog ids; empty is every cog.
    #[serde(default)]
    pub cogs: Vec<String>,
    /// Projects of the requesting cog; empty is every project.
    #[serde(default)]
    pub projects: Vec<String>,
    /// Shown to the cog and in the activity log.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `cogbus-policy.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    /// Cross-project grants.
    #[serde(default)]
    pub grants: Vec<GrantEntry>,
    /// Deny rules.
    #[serde(default)]
    pub deny: Vec<DenyEntry>,
}

fn valid_pattern(p: &str) -> bool {
    p == "*"
        || p.split_once('/').is_some_and(|(k, s)| {
            clawft_types::cog_topics::valid_topic_kind(k) && (s == "*" || weftos_cogbus::broker::valid_source_id(s))
        })
}

impl PolicyFile {
    /// Check every entry; the error names the first bad one.
    pub fn validate(&self) -> Result<(), String> {
        if self.grants.len() > MAX_ENTRIES || self.deny.len() > MAX_ENTRIES {
            return Err(format!("more than {MAX_ENTRIES} grants or deny rules"));
        }
        for (i, g) in self.grants.iter().enumerate() {
            if g.to.trim().is_empty() || g.from.trim().is_empty() || g.to == g.from {
                return Err(format!("grants[{i}]: `to` and `from` must be two different projects"));
            }
            if g.topics.is_empty() {
                return Err(format!("grants[{i}]: `topics` is empty"));
            }
            if let Some(t) = g.topics.iter().find(|t| !valid_pattern(t)) {
                return Err(format!("grants[{i}]: bad topic pattern {t:?}"));
            }
        }
        for (i, d) in self.deny.iter().enumerate() {
            if let Some(a) = d.actions.iter().find(|a| !ACTIONS.contains(&a.as_str())) {
                return Err(format!("deny[{i}]: unknown action {a:?} (one of {ACTIONS:?})"));
            }
            if let Some(t) = d.topics.iter().find(|t| !valid_pattern(t)) {
                return Err(format!("deny[{i}]: bad topic pattern {t:?}"));
            }
        }
        Ok(())
    }
}

/// Split `kind/source`, `kind/*` or a bare kind (a declare).
fn split(topic: &str) -> (&str, Option<&str>) {
    match topic.split_once('/') {
        Some((k, "*")) => (k, None),
        Some((k, s)) => (k, Some(s)),
        None => (topic, None),
    }
}

/// Does a deny pattern cover `topic`? A request for every source of a kind
/// (a wildcard subscribe, a declare) is covered by any pattern of the kind.
fn covers(pattern: &str, topic: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let ((pk, ps), (tk, ts)) = (split(pattern), split(topic));
    pk == tk && (ps.is_none() || ts.is_none() || ps == ts)
}

fn listed(list: &[String], v: Option<&str>) -> bool {
    list.is_empty() || v.is_some_and(|v| list.iter().any(|x| x == v))
}

impl DenyEntry {
    fn matches(&self, r: &BusRequest<'_>) -> bool {
        listed(&self.actions, Some(r.action.as_str()))
            && (self.topics.is_empty() || self.topics.iter().any(|p| covers(p, r.topic)))
            && listed(&self.cogs, Some(r.cog_id))
            && listed(&self.projects, r.project_id)
    }
}

impl PolicyFile {
    /// The first deny rule matching `r`, as a reason.
    pub fn denied(&self, r: &BusRequest<'_>) -> Option<String> {
        self.deny.iter().position(|d| d.matches(r)).map(|i| {
            let why = self.deny[i].reason.as_deref().unwrap_or("operator deny rule");
            format!("{POLICY_FILE} deny[{i}]: {why}")
        })
    }

    /// Grants for a subscribe `r` (its kind only).
    pub fn grants_for(&self, r: &BusRequest<'_>) -> Vec<Grant> {
        let Some(project) = r.project_id else { return Vec::new() };
        let (kind, _) = split(r.topic);
        let mut out = Vec::new();
        for g in self.grants.iter().filter(|g| g.to == project && listed(&g.cogs, Some(r.cog_id))) {
            for p in &g.topics {
                let (pk, ps) = split(p);
                if p == "*" || pk == kind {
                    out.push(Grant { project: g.from.clone(), source: if p == "*" { None } else { ps.map(String::from) } });
                }
            }
        }
        out
    }
}

/// Kernel rules as `(rules, risk threshold, human approval)`.
pub type Rules = (Vec<GovernanceRule>, f64, bool);

/// The file as last read: its stamp and what it said.
type Loaded = (Option<(SystemTime, u64)>, Result<PolicyFile, String>);

/// The daemon's cogbus policy.
pub struct CogbusPolicy {
    path: PathBuf,
    dir: PathBuf,
    loaded: Mutex<Loaded>,
    engine: Option<GovernanceEngine>,
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    std::fs::metadata(path).ok().map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
}

fn read(dir: &Path) -> Result<PolicyFile, String> {
    let f: PolicyFile = crate::workload_place_policy::parse(dir, POLICY_FILE)?.unwrap_or_default();
    f.validate().map_err(|e| format!("{POLICY_FILE}: {e}"))?;
    Ok(f)
}

impl CogbusPolicy {
    /// Policy over `dir`'s file and `rules` (none: the file only).
    pub fn new(dir: &Path, rules: Option<Rules>) -> Self {
        let engine = rules.map(|(rules, threshold, human)| {
            let mut e = GovernanceEngine::new(threshold, human);
            for r in rules {
                e.add_rule(r);
            }
            e
        });
        let path = dir.join(POLICY_FILE);
        let loaded = (stamp(&path), read(dir));
        if let Err(e) = &loaded.1 {
            tracing::error!(error = %e, "cogbus policy unreadable: every cogbus request is refused until it is fixed");
        }
        Self { path, dir: dir.to_path_buf(), loaded: Mutex::new(loaded), engine }
    }

    /// The file, re-read if it changed since the last call.
    fn file(&self) -> Result<PolicyFile, String> {
        let mut g = self.loaded.lock().unwrap_or_else(|p| p.into_inner());
        let now = stamp(&self.path);
        if now != g.0 {
            *g = (now, read(&self.dir));
            match &g.1 {
                Ok(_) => tracing::info!(path = %self.path.display(), "cogbus policy reloaded"),
                Err(e) => tracing::error!(error = %e, "cogbus policy unreadable: every cogbus request is refused until it is fixed"),
            }
        }
        g.1.clone()
    }

    fn kernel(&self, r: &BusRequest<'_>) -> Result<(), String> {
        let Some(engine) = &self.engine else { return Ok(()) };
        let ask = BusAsk { instance: r.instance_id, action: r.action.as_str(), topic: r.topic, cog_id: r.cog_id, schema: r.schema };
        crate::cogbus_governance::evaluate(engine, ask)
    }
}

impl BusPolicy for CogbusPolicy {
    fn decide(&self, r: &BusRequest<'_>) -> Result<(), String> {
        if r.action == BusAction::Unsubscribe {
            return Ok(());
        }
        let file = self.file()?;
        if let Some(why) = file.denied(r) {
            return Err(why);
        }
        self.kernel(r)
    }

    fn grants(&self, r: &BusRequest<'_>) -> Vec<Grant> {
        self.file().map(|f| f.grants_for(r)).unwrap_or_default()
    }
}

#[cfg(test)]
#[path = "cogbus_policy_tests.rs"]
mod tests;
