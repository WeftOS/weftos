//! The `agent.install` dashboard action (ADR-121 O3): the dashboard approved an
//! install request and queued it for this node. The node confirms locally and
//! installs an agent package or team into a project checkout registered here.
//!
//! The package comes from this node's own agent tree (the release's embedded
//! packages, or a `--from` style directory for tests and sideloads) and must
//! match the pinned bundle digest in the payload before anything is written;
//! a mismatch is a refusal, not a failure to retry. The install is
//! `weftos init`'s own plan and apply (never `--force`: drifted or unmanaged
//! files are refused, not overwritten), re-planned afterwards to check that
//! disk now matches the render, and chained (`agent.install` or
//! `agent.install.refused`, source `agent.install`).
//!
//! The payload is untrusted: every field is checked before the disk is
//! touched, and it is never echoed back in an error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use clawft_kernel::chain::ChainManager;
use serde::Deserialize;
use serde_json::{Value, json};
use weftos::init::Host;
use weftos::init::digest::Subject;
use weftos::init::team::{ApplyRequest, apply_request};

use crate::dashboard_actions::{Action, ActionHandler, ActionOutcome};

/// The action kind.
pub const KIND: &str = "agent.install";
/// Chain source for the records.
pub const CHAIN_SOURCE: &str = "agent.install";
pub const CHAIN_INSTALLED: &str = "agent.install";
pub const CHAIN_REFUSED: &str = "agent.install.refused";

/// The payload as the dashboard queues it (`decide_agent_install`). Field names
/// carry no `key`, `token`, `secret` or `password`: the dashboard refuses those.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInstallPayload {
    pub request_id: String,
    pub project_ulid: String,
    /// `agent` or `team`.
    pub subject: String,
    pub name: String,
    pub version: String,
    /// sha256 of the bundle ([`bundle_digest`]); the pin.
    pub digest: String,
    pub hosts: Vec<String>,
}

/// A request this node refuses or cannot complete.
#[derive(Debug)]
pub(crate) struct Refusal {
    /// Refused on purpose (bad request, digest mismatch, drifted files) as
    /// opposed to an operational failure.
    pub(crate) refused: bool,
    pub(crate) message: String,
}

pub(crate) fn refuse(m: impl Into<String>) -> Refusal {
    Refusal { refused: true, message: m.into() }
}

pub(crate) fn fail(m: impl Into<String>) -> Refusal {
    Refusal { refused: false, message: m.into() }
}

pub(crate) fn uuid_ok(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

pub(crate) fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A checked payload.
struct Checked {
    subject: Subject,
    hosts: Vec<Host>,
}

impl AgentInstallPayload {
    /// The pinned digest without its `sha256:` prefix (the catalog form).
    fn pin_hex(&self) -> &str {
        self.digest.strip_prefix("sha256:").unwrap_or(&self.digest)
    }

    fn check(&self) -> Result<Checked, Refusal> {
        if !uuid_ok(&self.request_id) {
            return Err(refuse("request_id is not a UUID"));
        }
        if !crate::mesh_pair_requests::is_ulid(&self.project_ulid) {
            return Err(refuse("project_ulid is not a ULID"));
        }
        let subject = Subject::parse(&self.subject).ok_or_else(|| refuse("subject must be agent or team"))?;
        if !name_ok(&self.name) {
            return Err(refuse("name is not a package name"));
        }
        if self.version.is_empty() || self.version.len() > 60 || self.version.chars().any(|c| c.is_control()) {
            return Err(refuse("version must be 1-60 printable characters"));
        }
        let pin = self.pin_hex();
        if pin.len() != 64 || !pin.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            return Err(refuse("digest must be sha256:<64 lowercase hex> (or the bare hex)"));
        }
        if self.hosts.is_empty() || self.hosts.len() > 3 {
            return Err(refuse("hosts must name one to three of claude, grok, codex"));
        }
        let mut hosts = Vec::new();
        for h in &self.hosts {
            let host = match h.as_str() {
                "claude" => Host::Claude,
                "grok" => Host::Grok,
                "codex" => Host::Codex,
                _ => return Err(refuse("hosts must name one to three of claude, grok, codex")),
            };
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        Ok(Checked { subject, hosts })
    }
}

/// Where the agent packages come from.
#[derive(Debug, Clone)]
pub enum PackageSource {
    /// The tree compiled into this binary: the release's own packages.
    Embedded,
    /// An `agents/` directory on disk.
    Dir(PathBuf),
}

impl PackageSource {
    #[cfg(test)]
    pub(crate) fn clone_for_test(&self) -> Self {
        self.clone()
    }
}

/// Everything the handler needs from the machine, injected so tests never
/// touch the real home or chain.
pub struct AgentInstallEnv {
    /// `~/.weftos/projects`: the project must be registered here.
    pub manifests_dir: PathBuf,
    pub source: PackageSource,
    /// The daemon's chain; `None` only in a build without one (a warning is logged).
    pub chain: Option<Arc<ChainManager>>,
}

impl AgentInstallEnv {
    /// The running user's manifest store and the embedded packages.
    pub fn from_home(home: &Path, chain: Option<Arc<ChainManager>>) -> Self {
        Self { manifests_dir: crate::user_daemon::manifests_dir(home), source: PackageSource::Embedded, chain }
    }

    fn record(&self, kind: &str, record: Value) {
        match &self.chain {
            Some(cm) => {
                cm.append(CHAIN_SOURCE, kind, Some(record));
            }
            None => tracing::warn!(kind, "no chain on this node: agent install not chained"),
        }
    }
}

/// Answers `agent.install` actions.
pub struct AgentInstallHandler {
    env: Option<Arc<AgentInstallEnv>>,
}

impl AgentInstallHandler {
    pub fn new(env: AgentInstallEnv) -> Self {
        Self { env: Some(Arc::new(env)) }
    }

    /// For `default_handlers()`: fails clearly when there is no home directory.
    /// The daemon replaces it with one that holds its chain.
    pub fn from_process() -> Self {
        Self { env: clawft_types::runtime_paths::home_dir().map(|h| Arc::new(AgentInstallEnv::from_home(&h, None))) }
    }
}

#[async_trait]
impl ActionHandler for AgentInstallHandler {
    fn kind(&self) -> &str {
        KIND
    }

    async fn handle(&self, action: &Action) -> ActionOutcome {
        let Some(env) = self.env.clone() else { return outcome(Err(fail("no home directory on this node")), None) };
        let payload = action.payload.clone();
        tokio::task::spawn_blocking(move || apply_action(&env, payload))
            .await
            .unwrap_or_else(|e| outcome(Err(fail(format!("agent install did not finish: {e}"))), None))
    }
}

fn outcome(r: Result<Value, Refusal>, request_id: Option<&str>) -> ActionOutcome {
    match r {
        Ok(result) => ActionOutcome { status: "succeeded", result },
        Err(Refusal { refused, message }) => ActionOutcome {
            status: "failed",
            result: json!({ "error": message, "kind": KIND, "refused": refused, "request_id": request_id }),
        },
    }
}

/// Validate, verify the digest, install, verify, chain.
pub fn apply_action(env: &AgentInstallEnv, payload: Value) -> ActionOutcome {
    let p: AgentInstallPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(_) => return outcome(Err(refuse("payload is not a valid agent install request")), None),
    };
    // Only a checked id is echoed back.
    let request_id = uuid_ok(&p.request_id).then(|| p.request_id.clone());
    let result = install(env, &p);
    if let Err(r) = &result
        && r.refused
    {
        env.record(
            CHAIN_REFUSED,
            json!({ "request_id": request_id, "project_ulid": p.project_ulid.chars().take(26).collect::<String>(),
                    "subject": p.subject.chars().take(8).collect::<String>(), "name": p.name.chars().take(64).collect::<String>(),
                    "digest": p.digest.chars().take(64).collect::<String>(), "reason": r.message }),
        );
    }
    outcome(result, request_id.as_deref())
}

fn install(env: &AgentInstallEnv, p: &AgentInstallPayload) -> Result<Value, Refusal> {
    let checked = p.check()?;
    let manifest = clawft_types::project::find_by_id(&env.manifests_dir, &p.project_ulid)
        .map_err(|e| fail(format!("cannot read the project index: {e}")))?
        .ok_or_else(|| refuse("this project is not registered on this node"))?;
    let root = manifest.root;
    if !root.is_dir() {
        return Err(refuse("the project's checkout is not on disk"));
    }

    // One code path with `weftos team apply`: digest pin, collision refusal,
    // write, verify by re-plan.
    let req = ApplyRequest {
        root: root.clone(),
        subject: checked.subject,
        name: p.name.clone(),
        hosts: checked.hosts.clone(),
        from: match &env.source {
            PackageSource::Dir(d) => Some(d.clone()),
            PackageSource::Embedded => None,
        },
        pin: Some(p.pin_hex().to_string()),
        dry_run: false,
    };
    let applied = apply_request(&req).map_err(|e| Refusal { refused: e.refused, message: e.message })?;
    let (first, touched, already) = (applied.plan, applied.touched, applied.already);

    let hosts: Vec<&str> = checked.hosts.iter().map(|h| h.as_str()).collect();
    env.record(
        CHAIN_INSTALLED,
        json!({ "request_id": p.request_id, "project_ulid": p.project_ulid, "subject": p.subject, "name": p.name,
                "version": p.version, "digest": p.digest, "hosts": hosts, "packages": first.selected,
                "files": touched, "already_installed": already, "source_commit": first.commit }),
    );
    Ok(json!({ "request_id": p.request_id, "subject": p.subject, "name": p.name, "version": p.version, "digest": p.digest,
               "hosts": hosts, "root": root, "packages": first.selected, "files": touched, "already_installed": already }))
}

#[cfg(test)]
#[path = "agent_install_tests.rs"]
mod tests;
