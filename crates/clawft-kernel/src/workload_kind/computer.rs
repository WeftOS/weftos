//! The `computer` kind (owner decisions D1, D2, D5 of 2026-11-09): a desktop
//! that a bot may work on, reached through the daemon-hosted computer hub.
//!
//! Like [`project`](super::project), a computer has no signed package: there is
//! no binary to verify, only a declaration of which desktop it is, what kind of
//! host it is, what the bot may do there and under which access rule. The
//! manifest body is checked here ([`ComputerBody::parse`], used by
//! [`WorkloadKind::validate`]); [`WorkloadKind::load`] always refuses a package
//! directory, and the daemon builds the workload from the validated envelope
//! with [`computer_workload`].
//!
//! # Body
//!
//! ```json
//! { "desktop": "xpc", "host_kind": "desktop",
//!   "capabilities": ["screen.capture", "input.type"],
//!   "access": "own" }
//! { "desktop": "macbook", "host_kind": "desktop",
//!   "capabilities": ["screen.capture", "input.click"],
//!   "access": "granted",
//!   "allowlist": { "apps": ["Terminal"], "displays": ["main"] } }
//! ```
//!
//! # Access (D5)
//!
//! * `own`: the bot's own computer. Full access, no per-action ask. An
//!   allowlist on an `own` computer is refused (it would contradict the
//!   access).
//! * `granted`: real desktop control of a desktop the bot was granted. Every
//!   action needs a per-action ask (R9 `escalate_to_human`) **and** the target
//!   app and display must be on the allowlist; anything outside the allowlist
//!   is denied, not asked. A `granted` manifest must carry both lists, each
//!   non-empty.
//!
//! [`ComputerBody::verdict`] is that rule as code.
//!
//! A desktop's state is bound to its hardware, so [`ComputerKind::migratable`]
//! is `false`: losing the node raises an alert, it does not reschedule.

#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use std::path::Path;

use serde::{Deserialize, Serialize};

#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use clawft_types::placement::engine::{PlacementPolicy, WorkloadRequirements, WorkloadSpec};
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use clawft_types::placement::{CapabilityId, MemoryDemand, Requirement};

#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use crate::workload_ctl::PlaneError;
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use crate::workload_pkg::TrustAnchors;
use crate::workload_pkg::manifest::valid_token;
use crate::workload_pkg::{ManifestEnvelope, ManifestError};
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use crate::workload_runtime::{ComputerPayload, VerifiedWorkload, WorkloadSource};

use super::WorkloadKind;
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use super::KindRegistry;

/// Manifest kind string.
pub const KIND_COMPUTER: &str = "computer";

/// Longest desktop id.
pub const MAX_DESKTOP_LEN: usize = 64;
/// Most capabilities one manifest may list.
pub const MAX_CAPABILITIES: usize = 32;
/// Most entries in one allowlist.
pub const MAX_ALLOWLIST: usize = 64;
/// Longest capability, app or display token.
pub const MAX_TOKEN_LEN: usize = 128;

/// `host_kind` values, matching the hub's (`desktop`, `container`, `sandbox`).
pub const HOST_KINDS: [&str; 3] = ["desktop", "container", "sandbox"];

/// Who the computer belongs to, and so how a bot may use it (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// The bot's own computer: full access, no per-action ask.
    Own,
    /// A granted desktop: per-action ask plus an app and display allowlist.
    Granted,
}

/// App and display allowlist of a `granted` computer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowlist {
    /// Application names the bot may act in.
    #[serde(default)]
    pub apps: Vec<String>,
    /// Display ids the bot may act on.
    #[serde(default)]
    pub displays: Vec<String>,
}

/// What a bot's attempted action on a computer needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Proceed; no ask.
    Allow,
    /// Needs a per-action ask (R9 `escalate_to_human`) before it runs.
    Ask,
    /// Outside the allowlist: refused without asking.
    Deny,
}

/// The validated `computer` manifest body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerBody {
    /// The desktop's machine id on the hub (`[a-z0-9][a-z0-9_-]*`).
    pub desktop: String,
    /// `desktop`, `container` or `sandbox` (the minter's `host_kind`).
    pub host_kind: String,
    /// What the bot may do there, as tokens (`screen.capture`, ...).
    pub capabilities: Vec<String>,
    /// The access rule.
    pub access: Access,
    /// Required (non-empty lists) for `granted`, refused for `own`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowlist: Option<Allowlist>,
}

fn invalid(msg: impl Into<String>) -> ManifestError {
    ManifestError::Invalid(msg.into())
}

/// A desktop id is also a placement capability segment.
fn valid_desktop(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= MAX_DESKTOP_LEN
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

fn check_tokens(what: &str, items: &[String], max_items: usize, allow_empty: bool) -> Result<(), ManifestError> {
    if items.is_empty() && !allow_empty {
        return Err(invalid(format!("{what} must not be empty")));
    }
    if items.len() > max_items {
        return Err(invalid(format!("{what} lists more than {max_items} entries")));
    }
    for (i, t) in items.iter().enumerate() {
        // Apps may carry spaces ("Google Chrome"): printable, no control characters.
        let ok = !t.is_empty()
            && t.len() <= MAX_TOKEN_LEN
            && t.chars().all(|c| !c.is_control())
            && t.trim() == t;
        if !ok {
            return Err(invalid(format!("{what}[{i}] is not a short printable name")));
        }
        if items[..i].contains(t) {
            return Err(invalid(format!("{what} lists {:?} twice", t.chars().take(40).collect::<String>())));
        }
    }
    Ok(())
}

impl ComputerBody {
    /// Parse and validate a manifest body. A malformed body is refused with a
    /// reason, never defaulted.
    pub fn parse(body: &serde_json::Value) -> Result<Self, ManifestError> {
        let b: Self = serde_json::from_value(body.clone())
            .map_err(|e| ManifestError::Parse(format!("computer body: {e}")))?;
        b.validate()?;
        Ok(b)
    }

    /// Check every field and the access rule.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !valid_desktop(&self.desktop) {
            return Err(invalid(format!(
                "desktop {:?} must match [a-z0-9][a-z0-9_-]* and be at most {MAX_DESKTOP_LEN} characters",
                self.desktop.chars().take(40).collect::<String>()
            )));
        }
        if !HOST_KINDS.contains(&self.host_kind.as_str()) {
            return Err(invalid(format!(
                "host_kind {:?} is not one of desktop, container, sandbox",
                self.host_kind.chars().take(40).collect::<String>()
            )));
        }
        if self.capabilities.is_empty() || self.capabilities.len() > MAX_CAPABILITIES {
            return Err(invalid(format!("capabilities must list 1 to {MAX_CAPABILITIES} entries")));
        }
        for (i, c) in self.capabilities.iter().enumerate() {
            if !valid_token(c, MAX_TOKEN_LEN) {
                return Err(invalid(format!("capabilities[{i}] is not a valid token")));
            }
            if self.capabilities[..i].contains(c) {
                return Err(invalid(format!("capabilities lists {c:?} twice")));
            }
        }
        match (self.access, &self.allowlist) {
            (Access::Own, None) => {}
            (Access::Own, Some(_)) => {
                return Err(invalid(
                    "an `own` computer has full access: an allowlist contradicts it (use `granted`)",
                ));
            }
            (Access::Granted, None) => {
                return Err(invalid("a `granted` computer needs an allowlist of apps and displays"));
            }
            (Access::Granted, Some(a)) => {
                check_tokens("allowlist.apps", &a.apps, MAX_ALLOWLIST, false)?;
                check_tokens("allowlist.displays", &a.displays, MAX_ALLOWLIST, false)?;
            }
        }
        Ok(())
    }

    /// The D5 rule for one attempted action on `app` and `display`.
    ///
    /// `own`: [`Verdict::Allow`]. `granted`: [`Verdict::Deny`] unless both the
    /// app and the display are on the allowlist, in which case
    /// [`Verdict::Ask`] (every action is asked, none is remembered).
    pub fn verdict(&self, app: &str, display: &str) -> Verdict {
        match (&self.access, &self.allowlist) {
            (Access::Own, _) => Verdict::Allow,
            (Access::Granted, Some(a))
                if a.apps.iter().any(|x| x == app) && a.displays.iter().any(|x| x == display) =>
            {
                Verdict::Ask
            }
            (Access::Granted, _) => Verdict::Deny,
        }
    }
}

/// Computers: desktops reached through the computer hub.
#[derive(Debug, Clone, Copy, Default)]
pub struct ComputerKind;

impl WorkloadKind for ComputerKind {
    fn id(&self) -> &'static str {
        KIND_COMPUTER
    }

    fn health(&self) -> super::HealthSpec {
        // Judged by the hub's heartbeat (the tool server pings every 30 s).
        super::HealthSpec {
            interval_ms: 15_000,
            miss_limit: 3,
        }
    }

    /// Health is the hub heartbeat: the adapter reports how long ago the
    /// desktop's tool server was last heard from, and a gap longer than
    /// `interval_ms * miss_limit` is unhealthy. A desktop with no hub
    /// connection at all is unhealthy under its own reason. The supervisor
    /// still counts `miss_limit` consecutive unhealthy polls before it acts,
    /// so this is the evidence window, not the restart trigger.
    fn judge(&self, sample: &super::HealthSample) -> super::Health {
        use super::{Health, SampleState};
        match sample.state {
            SampleState::Running | SampleState::Degraded | SampleState::Unknown => {
                let spec = self.health();
                let limit = spec.interval_ms.saturating_mul(u64::from(spec.miss_limit));
                match sample.heartbeat_age_ms {
                    None => Health::Unhealthy("no hub connection for this desktop".into()),
                    Some(age) if age > limit => Health::Unhealthy(format!(
                        "hub heartbeat stale: last seen {}s ago (limit {}s)",
                        age / 1000,
                        limit / 1000
                    )),
                    Some(_) => Health::Healthy,
                }
            }
            // Never started, or ended: the process rules apply.
            _ => super::judge_process(sample),
        }
    }

    /// A desktop's state is bound to its hardware: never moved.
    fn migratable(&self) -> bool {
        false
    }

    /// Alert, never restart (owner decision 4, 2026-11-15): restarting the
    /// adapter's watch cannot make a silent desktop answer, and a restart
    /// loop would hide the outage. The instance goes `Unhealthy` with the
    /// named reason, chained as an alert, and returns to `Running` when the
    /// desktop pings again.
    fn restart_on_unhealthy(&self) -> bool {
        false
    }

    fn validate(&self, envelope: &ManifestEnvelope) -> Result<(), ManifestError> {
        if envelope.kind != KIND_COMPUTER {
            return Err(invalid(format!("kind {:?} is not {KIND_COMPUTER:?}", envelope.kind)));
        }
        ComputerBody::parse(&envelope.body).map(|_| ())
    }

    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    fn load(
        &self,
        _package_dir: &Path,
        _anchors: &TrustAnchors,
        _kinds: &KindRegistry,
    ) -> Result<VerifiedWorkload, PlaneError> {
        Err(PlaneError::Package(
            "computer workloads have no signed package: the daemon builds them from a validated \
             manifest (`computer_workload`)"
                .into(),
        ))
    }

    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    fn spec(&self, workload: &VerifiedWorkload) -> Result<WorkloadSpec, String> {
        let WorkloadSource::Computer(c) = &workload.source else {
            return Err("not a computer workload".into());
        };
        // Pinned to the named desktop: only the node that is that desktop
        // advertises this capability.
        let id = CapabilityId::new(format!("{CAP_COMPUTER_PREFIX}{}", c.desktop)).map_err(|e| e.to_string())?;
        let spec = WorkloadSpec {
            kind: workload.kind.clone(),
            name: workload.id.clone(),
            requirements: WorkloadRequirements {
                common: vec![Requirement::exact(id)],
                variants: Vec::new(),
                memory: MemoryDemand::default(),
            },
            policy: PlacementPolicy::default(),
        };
        spec.validate().map_err(|e| e.to_string())?;
        Ok(spec)
    }

    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    fn adapters(&self) -> &'static [&'static str] {
        &["computer"]
    }
}

/// Capability a node advertises when it is the desktop `<id>`.
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
pub const CAP_COMPUTER_PREFIX: &str = "computer.desktop.";

/// Build the workload for a validated `computer` envelope. Authority is the
/// operator's own declaration (like `project`, there is no package to sign), so
/// this refuses anything but a `computer` envelope whose body passes
/// [`ComputerBody::parse`].
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
pub fn computer_workload(envelope: &ManifestEnvelope) -> Result<VerifiedWorkload, ManifestError> {
    ComputerKind.validate(envelope)?;
    let b = ComputerBody::parse(&envelope.body)?;
    Ok(VerifiedWorkload {
        kind: KIND_COMPUTER.to_string(),
        id: b.desktop.clone(),
        version: "1".to_string(),
        source: WorkloadSource::Computer(Box::new(ComputerPayload {
            desktop: b.desktop,
            host_kind: b.host_kind,
            capabilities: b.capabilities,
            access: b.access,
            allowlist: b.allowlist,
        })),
    })
}
