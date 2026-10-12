//! `.weftos/agents.lock.json`: which files `weftos init` owns, their hashes,
//! and the weftos version and commit they were rendered from. Serialization
//! is deterministic (sorted, no timestamps) so an unchanged re-run leaves the
//! lock byte-identical.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::InitError;
use super::render::{Host, Merge};

pub const LOCK_PATH: &str = ".weftos/agents.lock.json";
pub const SCHEMA: u32 = 1;
pub const RENDERER: &str = "weftos-init/1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockFile {
    pub path: String,
    pub host: Host,
    pub sha256: String,
    pub merge: Merge,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockAgent {
    pub id: String,
    pub weftos_version: String,
    pub commit: String,
    /// Package version (manifest `version`, else the weftos version).
    #[serde(default)]
    pub version: String,
    /// Bundle digest of the package as applied ([`super::digest::package_digest`]).
    #[serde(default)]
    pub digest: String,
    pub files: Vec<LockFile>,
}

/// One member slot of an applied team.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockSlot {
    pub agent: String,
    #[serde(default)]
    pub position: Option<u32>,
    pub active: bool,
}

/// A team applied to this project.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockTeam {
    pub name: String,
    /// Bundle digest of the team as applied ([`super::digest::team_digest`]).
    pub digest: String,
    pub version: String,
    pub hosts: Vec<Host>,
    pub slots: Vec<LockSlot>,
}

/// The project-context file apply seeded (removed again only if unmodified).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockContext {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lock {
    pub schema: u32,
    pub renderer: String,
    pub weftos_version: String,
    /// Commit of the agents source (build commit when embedded).
    pub commit: String,
    pub source: String,
    #[serde(default)]
    pub team: Option<String>,
    #[serde(default)]
    pub agents: Vec<LockAgent>,
    #[serde(default)]
    pub teams: Vec<LockTeam>,
    #[serde(default)]
    pub context: Option<LockContext>,
}

impl Lock {
    pub fn empty() -> Self {
        Self {
            schema: SCHEMA,
            renderer: RENDERER.into(),
            weftos_version: String::new(),
            commit: String::new(),
            source: String::new(),
            team: None,
            agents: Vec::new(),
            teams: Vec::new(),
            context: None,
        }
    }

    pub fn read(root: &Path) -> Result<Option<Self>, InitError> {
        let path = root.join(LOCK_PATH);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| InitError::Other(format!("{}: {e}", path.display())))
    }

    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).expect("lock serializes");
        s.push('\n');
        s
    }

    /// `path -> (owner, file)` over every managed file.
    pub fn files_by_path(&self) -> BTreeMap<&str, (&str, &LockFile)> {
        self.agents
            .iter()
            .flat_map(|a| {
                a.files
                    .iter()
                    .map(move |f| (f.path.as_str(), (a.id.as_str(), f)))
            })
            .collect()
    }

    /// Agent ids that have files for `host`.
    pub fn agents_for(&self, host: Host) -> Vec<String> {
        self.agents
            .iter()
            .filter(|a| a.files.iter().any(|f| f.host == host))
            .map(|a| a.id.clone())
            .collect()
    }

    /// Nothing left that apply owns.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty() && self.teams.is_empty() && self.context.is_none()
    }

    /// Sort agents and files so serialization is stable.
    pub fn normalize(&mut self) {
        for a in &mut self.agents {
            a.files.sort_by(|x, y| x.path.cmp(&y.path));
        }
        self.agents.retain(|a| !a.files.is_empty());
        self.agents.sort_by(|x, y| x.id.cmp(&y.id));
        self.teams.sort_by(|x, y| x.name.cmp(&y.name));
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
