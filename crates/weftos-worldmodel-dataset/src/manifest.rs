//! The `manifest.json` written beside an archived session.

use crate::evidence::{EvidenceSummary, TypeCounts};
use crate::layout::SessionLayout;
use crate::proof::ProofCounts;
use crate::ruview::CsiRecording;
use crate::senml::ReadingsSummary;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MANIFEST_VERSION: &str = "lewm.dataset.v1";
pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// `evidence`, `csi`, `csi_meta`, `readings`, `protocol` or `consent`.
    pub role: String,
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Archiver {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub min_t_ns: u64,
    pub max_t_ns: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeSpan {
    pub evidence: Span,
    pub readings: Option<Span>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceStats {
    pub lines: u64,
    pub by_type: BTreeMap<String, TypeCounts>,
    pub proof_total: ProofCounts,
    pub unknown_types: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadingsStats {
    pub packs: u64,
    pub records: u64,
    pub proof: ProofCounts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: String,
    pub session: String,
    /// Region id taken from the evidence lines. With several regions (only
    /// when the archive was made with `allow_multi_region`) the first, sorted.
    pub room: String,
    /// Every region in the evidence, sorted. Empty in manifests written before
    /// this field existed, which held exactly one region, `room`.
    #[serde(default)]
    pub regions: Vec<String>,
    pub created_at: String,
    pub archiver: Archiver,
    pub layout: SessionLayout,
    pub files: Vec<FileEntry>,
    /// Roles that were expected but not found: `csi`, `readings`.
    pub missing: Vec<String>,
    pub time_span: TimeSpan,
    pub evidence: EvidenceStats,
    pub readings: Option<ReadingsStats>,
    /// `shell_measure` records, verbatim.
    pub shell_measure: Vec<Value>,
    /// `pose` records keyed by `node_id`, last wins, verbatim.
    pub node_poses: BTreeMap<String, Value>,
    /// The RuView recording sidecar (`<session>.csi.meta.json`), when present.
    #[serde(default)]
    pub csi_recording: Option<CsiRecording>,
    pub protocol: Option<Value>,
    pub consent: Value,
    /// Whether the session's finished marker was present when archived.
    #[serde(default)]
    pub finished: bool,
}

pub(crate) struct ManifestInputs {
    pub session: String,
    pub layout: SessionLayout,
    pub files: Vec<FileEntry>,
    pub evidence: EvidenceSummary,
    pub readings: Option<ReadingsSummary>,
    pub csi_recording: Option<CsiRecording>,
    pub protocol: Option<Value>,
    pub consent: Value,
    pub finished: bool,
}

impl Manifest {
    pub(crate) fn build(inp: ManifestInputs) -> Self {
        let mut missing = Vec::new();
        for role in ["csi", "readings"] {
            if !inp.files.iter().any(|f| f.role == role) {
                missing.push(role.to_string());
            }
        }
        let readings_span = inp.readings.as_ref().and_then(|r| {
            Some(Span {
                min_t_ns: r.min_t_ns?,
                max_t_ns: r.max_t_ns?,
            })
        });
        let e = inp.evidence;
        Manifest {
            manifest_version: MANIFEST_VERSION.into(),
            session: inp.session,
            room: e.regions.first().cloned().unwrap_or_default(),
            regions: e.regions,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            archiver: Archiver {
                name: env!("CARGO_PKG_NAME").into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            layout: inp.layout,
            files: inp.files,
            missing,
            time_span: TimeSpan {
                evidence: Span {
                    min_t_ns: e.min_t_ns,
                    max_t_ns: e.max_t_ns,
                },
                readings: readings_span,
            },
            evidence: EvidenceStats {
                lines: e.lines,
                by_type: e.by_type,
                proof_total: e.proof_total,
                unknown_types: e.unknown_types,
            },
            readings: inp.readings.map(|r| ReadingsStats {
                packs: r.packs,
                records: r.records,
                proof: r.proof,
            }),
            shell_measure: e.shell_measures,
            node_poses: e.node_poses,
            csi_recording: inp.csi_recording,
            protocol: inp.protocol,
            consent: inp.consent,
            finished: inp.finished,
        }
    }
}
