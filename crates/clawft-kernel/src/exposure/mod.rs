//! Mesh exposures: what a node offers to others, announced, switchable and
//! logged (ADR-120 X1/X2).
//!
//! - [`ExposureRegistry`]: one registry per node (id, kind, resource,
//!   audience, api, limits, state). ADR-101's `expose_to_mesh` inference
//!   roles live in it as `infer.<role>`. Operator decisions (off, paused, a
//!   narrowed audience) are persisted and win over configuration.
//! - [`ExposureRegistry::check_serve`]: the single answer to "may this be
//!   served now". An exposure that is reachable but not announced is refused
//!   (ADR-101's `StoppedExposed` rule, generalised).
//! - [`ActivityLog`]: per-exposure record of requests (never bodies),
//!   hash-chained, retained for [`Retention::default`] (defaults pending the
//!   owner: 14 days or 50 MB per node, whichever first).
//!
//! Defaults pending the owner (ADR-120 open questions): audience `local`,
//! caller identity kept at node plus user label, the dashboard may switch an
//! exposure off but not on.

pub mod activity;
pub mod registry;

#[cfg(test)]
mod tests_activity;
#[cfg(test)]
mod tests_registry;

use serde::{Deserialize, Serialize};

pub use activity::{ActivityLog, ActivityRecord, NewActivity, Retention};
pub use registry::{AuditFn, ExposureRegistry, ExposureSpec};

/// What an exposure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExposureKind {
    /// Chat/completions style inference (`infer.<role>`).
    Inference,
    /// Text to speech.
    Tts,
    /// Speech to text.
    Stt,
    /// Embeddings.
    Embedding,
    /// A harness that can be started remotely (ADR-120 section 3).
    Harness,
    /// A cog's API.
    CogApi,
    /// A cogbus topic (`topic.<kind>/<source_id>`, ADR-122 section 14).
    Topic,
    /// Anything else.
    Other,
}

impl ExposureKind {
    /// Parse the CLI/RPC spelling (`inference`, `cog-api`, ...).
    pub fn parse(s: &str) -> Result<Self, String> {
        serde_json::from_value(serde_json::Value::String(s.to_owned()))
            .map_err(|_| format!("unknown exposure kind {s:?}"))
    }
}

/// Operator state of an exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExposureState {
    /// Served to its audience.
    On,
    /// Not served; the advertisement is withdrawn.
    Off,
    /// Temporarily not served (same effect as off, shown differently).
    Paused,
}

impl ExposureState {
    /// Lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Paused => "paused",
        }
    }
}

/// Who may use an exposure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Audience {
    /// This node only (the default; nothing leaves).
    Local,
    /// Paired peers holding a grant (ADR-108).
    Paired,
    /// These nodes.
    Nodes(Vec<String>),
    /// Members of a project scope.
    Project(String),
}

impl Audience {
    /// True if the exposure may leave this node at all.
    pub fn is_remote(&self) -> bool {
        !matches!(self, Self::Local)
    }

    /// Compact text for tables and the report (`local`, `paired`,
    /// `nodes(2)`, `project:<id>`).
    pub fn label(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Paired => "paired".into(),
            Self::Nodes(n) => format!("nodes({})", n.len()),
            Self::Project(p) => format!("project:{p}"),
        }
    }

    /// Parse `local`, `paired`, `nodes:a,b` or `project:<id>`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.split_once(':') {
            None if s == "local" => Ok(Self::Local),
            None if s == "paired" => Ok(Self::Paired),
            Some(("nodes", list)) => {
                let nodes: Vec<String> = list.split(',').map(str::trim).filter(|n| !n.is_empty()).map(String::from).collect();
                if nodes.is_empty() {
                    return Err("audience nodes: needs at least one node id".into());
                }
                Ok(Self::Nodes(nodes))
            }
            Some(("project", p)) if !p.trim().is_empty() => Ok(Self::Project(p.trim().to_owned())),
            _ => Err(format!("unknown audience {s:?} (local, paired, nodes:<a,b>, project:<id>)")),
        }
    }
}

/// Per-exposure limits. `None` is "no limit set", not "unlimited by policy".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Concurrent requests.
    pub max_concurrency: Option<u32>,
    /// Requests per minute.
    pub rate_per_min: Option<u32>,
    /// Largest request body in bytes.
    pub max_request_bytes: Option<u64>,
    /// Allowed local hours `[from, to)`, 0..=24.
    pub allowed_hours: Option<(u8, u8)>,
}

/// One exposure as the registry holds it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exposure {
    /// Stable name (`infer.chat`, `harness.metaharness`, ...).
    pub id: String,
    /// What it is.
    pub kind: ExposureKind,
    /// What it uses (`accel.gpu.cuda:0`, `accel.npu.ane`, `cpu`).
    pub resource: String,
    /// Estimated load on the resource, percent.
    pub load_pct: Option<u8>,
    /// Who may use it.
    pub audience: Audience,
    /// Protocol (`openai-chat`, `openai-audio`, `mcp`, `weftos-harness`).
    pub api: String,
    /// Limits.
    pub limits: Limits,
    /// Operator state.
    pub state: ExposureState,
    /// Whether an advertisement for it is currently published.
    pub announced: bool,
}

/// Why an exposure is not served now. Local policy blocking a remote
/// request shows as `refused` (ADR-120 section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `[mesh.expose] enabled = false`.
    MasterOff,
    /// No such exposure registered: reachable but not announced.
    Unregistered,
    /// Switched off locally.
    Off,
    /// Paused locally.
    Paused,
    /// Audience is `local`.
    LocalOnly,
    /// On, but no advertisement is published for it.
    NotAnnounced,
}

impl Refusal {
    /// Short reason for logs and CLI output.
    pub fn reason(self) -> &'static str {
        match self {
            Self::MasterOff => "mesh exposure is switched off on this node ([mesh.expose] enabled = false)",
            Self::Unregistered => "not registered, so not announced",
            Self::Off => "switched off locally",
            Self::Paused => "paused locally",
            Self::LocalOnly => "audience is local",
            Self::NotAnnounced => "reachable but not announced",
        }
    }
}
