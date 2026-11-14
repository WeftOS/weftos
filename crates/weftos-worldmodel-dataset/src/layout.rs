//! Session file naming. Defaults follow RuView's sensing-server recording code
//! (verified 2026-11-14, see the plan section 1b); every name stays a template
//! so a difference is a config change. `{session}` expands to the session id
//! and `*` matches any run of characters inside a file name.

use crate::error::DatasetError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLayout {
    /// Required SPATIAL evidence JSONL.
    pub evidence: String,
    /// Optional SenML readings JSONL. A WeftOS-side convention: RuView does
    /// not write a per-session readings file today.
    pub readings: String,
    /// Optional CSI recording patterns. Every match is archived. The first is
    /// RuView's exact name; the glob is the fallback for other recorders.
    pub csi: Vec<String>,
    /// The metadata sidecar RuView writes when a recording stops. Parsed into
    /// the manifest's `csi_recording` section and archived when present.
    #[serde(default = "default_csi_meta")]
    pub csi_meta: String,
    /// Optional protocol description, first existing candidate wins.
    pub protocol: Vec<String>,
    /// Optional consent record, first existing candidate wins.
    pub consent: Vec<String>,
    /// Optional finished marker: a file the recorder writes when the session
    /// is closed. `None` disables the marker.
    #[serde(default = "default_marker")]
    pub finished_marker: Option<String>,
    /// Refuse to archive a session whose finished marker is absent. A manifest
    /// written before this field existed loads as `false`.
    #[serde(default)]
    pub require_finished: bool,
}

/// RuView writes this at recording stop, so its presence means "finished".
const RUVIEW_META: &str = "{session}.csi.meta.json";

fn default_marker() -> Option<String> {
    Some(RUVIEW_META.into())
}

fn default_csi_meta() -> String {
    RUVIEW_META.into()
}

impl Default for SessionLayout {
    fn default() -> Self {
        Self {
            evidence: "{session}.evidence.jsonl".into(),
            readings: "{session}.readings.jsonl".into(),
            csi: vec!["{session}.csi.jsonl".into(), "{session}.csi*".into()],
            csi_meta: default_csi_meta(),
            protocol: vec![
                "{session}.protocol.json".into(),
                "{session}.protocol.txt".into(),
                "protocol.json".into(),
                "protocol.txt".into(),
            ],
            consent: vec!["{session}.consent.json".into(), "consent.json".into()],
            finished_marker: default_marker(),
            require_finished: true,
        }
    }
}

/// The files found for one session. Paths are absolute or as given.
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    pub id: String,
    pub evidence: PathBuf,
    pub csi: Vec<PathBuf>,
    pub readings: Option<PathBuf>,
    /// The RuView metadata sidecar, when it exists.
    pub csi_meta: Option<PathBuf>,
    pub protocol: Option<PathBuf>,
    pub consent: Option<PathBuf>,
    /// The finished marker, when the layout has one and it exists.
    pub finished: Option<PathBuf>,
}

/// Session ids become directory names, so they are restricted.
pub fn validate_session_id(id: &str) -> Result<(), DatasetError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(DatasetError::InvalidSession(id.to_string()))
    }
}

impl SessionLayout {
    /// Find the session's files in `dir`. With `session == None` the id is
    /// inferred from the single file matching the evidence template.
    pub fn discover(
        &self,
        dir: &Path,
        session: Option<&str>,
    ) -> Result<DiscoveredSession, DatasetError> {
        let names = list_names(dir)?;
        let id = match session {
            Some(s) => s.to_string(),
            None => self.infer_session(&names)?,
        };
        validate_session_id(&id)?;
        let expand = |t: &str| t.replace("{session}", &id);
        let evidence_name = expand(&self.evidence);
        if !names.contains(&evidence_name) {
            return Err(DatasetError::MissingEvidence(dir.join(evidence_name)));
        }
        let readings_name = expand(&self.readings);
        let marker_name = self.finished_marker.as_deref().map(expand);
        let meta_name = expand(&self.csi_meta);
        let csi: Vec<PathBuf> = names
            .iter()
            .filter(|n| **n != evidence_name && **n != readings_name && **n != meta_name)
            .filter(|n| marker_name.as_ref() != Some(*n))
            .filter(|n| self.csi.iter().any(|p| glob_match(&expand(p), n)))
            .map(|n| dir.join(n))
            .collect();
        let first_existing = |cands: &[String]| {
            cands
                .iter()
                .map(|c| expand(c))
                .find(|c| names.contains(c))
                .map(|c| dir.join(c))
        };
        let finished = self
            .finished_marker
            .as_deref()
            .map(expand)
            .filter(|m| names.contains(m))
            .map(|m| dir.join(m));
        if self.require_finished {
            let Some(template) = self.finished_marker.as_deref() else {
                return Err(DatasetError::BadLayout(
                    "finished_marker".into(),
                    "require_finished needs a finished_marker template".into(),
                ));
            };
            if finished.is_none() {
                return Err(DatasetError::NotFinished(dir.join(expand(template))));
            }
        }
        Ok(DiscoveredSession {
            finished,
            evidence: dir.join(&evidence_name),
            readings: names
                .contains(&readings_name)
                .then(|| dir.join(&readings_name)),
            csi,
            csi_meta: names.contains(&meta_name).then(|| dir.join(&meta_name)),
            protocol: first_existing(&self.protocol),
            consent: first_existing(&self.consent),
            id,
        })
    }

    fn infer_session(&self, names: &[String]) -> Result<String, DatasetError> {
        let suffix = self
            .evidence
            .strip_prefix("{session}")
            .filter(|s| !s.contains("{session}"))
            .ok_or_else(|| {
                DatasetError::SessionNotInferable(
                    "evidence template does not start with {session}; pass the id".into(),
                )
            })?;
        let ids: Vec<&str> = names
            .iter()
            .filter_map(|n| n.strip_suffix(suffix))
            .filter(|s| validate_session_id(s).is_ok())
            .collect();
        match ids.as_slice() {
            [one] => Ok((*one).to_string()),
            [] => Err(DatasetError::SessionNotInferable(format!(
                "no file ends with {suffix}"
            ))),
            many => Err(DatasetError::SessionNotInferable(format!(
                "several sessions found: {many:?}"
            ))),
        }
    }
}

fn list_names(dir: &Path) -> Result<Vec<String>, DatasetError> {
    let rd = std::fs::read_dir(dir).map_err(|e| DatasetError::io(dir, e))?;
    let mut names = Vec::new();
    for entry in rd {
        let entry = entry.map_err(|e| DatasetError::io(dir, e))?;
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if let (true, Some(n)) = (is_file, entry.file_name().to_str()) {
            names.push(n.to_string());
        }
    }
    names.sort();
    Ok(names)
}

/// Minimal glob: `*` matches any run (including empty), everything else literal.
pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !name.starts_with(first) || !name.ends_with(last) || name.len() < first.len() + last.len() {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob() {
        assert!(glob_match("s1.csi*", "s1.csi"));
        assert!(glob_match("s1.csi*", "s1.csi.bin"));
        assert!(glob_match("s1*.csi.*", "s1-node2.csi.dat"));
        assert!(!glob_match("s1*.csi.*", "s1.evidence.jsonl"));
        assert!(!glob_match("s1.csi*", "s10.csi"));
    }

    #[test]
    fn session_id_rules() {
        assert!(validate_session_id("room-1_2026.11").is_ok());
        assert!(validate_session_id("walk.v2-20261114_101500").is_ok());
        for bad in ["", ".hidden", "a/b", "..", "a b", "a\\b", "x\0y"] {
            assert!(validate_session_id(bad).is_err(), "{bad}");
        }
    }
}
