//! Envelope validation for `spatial.evidence.v1` JSONL (ADR-111 section 1.1).
//! Only the envelope is checked. Per-type geometry rules belong to the
//! reference validator in the spatial workspace.

use crate::error::DatasetError;
use crate::proof::ProofCounts;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// Record types defined by ADR-111 sections 1.3 and 1.4.
pub const KNOWN_TYPES: [&str; 13] = [
    "shell_measure",
    "pose",
    "radar_track_point",
    "rf_link_observation",
    "rf_gaussian",
    "uwb_echo",
    "imu_event",
    "human_confirm",
    "tof_depth",
    "radar_range",
    "acoustic_range",
    "radio_delay",
    "clock_correction",
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TypeCounts {
    pub lines: u64,
    pub proof: ProofCounts,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceSummary {
    pub lines: u64,
    pub region: String,
    pub min_t_ns: u64,
    pub max_t_ns: u64,
    pub by_type: BTreeMap<String, TypeCounts>,
    pub proof_total: ProofCounts,
    /// Lines whose `type` is not in [`KNOWN_TYPES`], counted by type.
    pub unknown_types: BTreeMap<String, u64>,
    /// `shell_measure` records, verbatim.
    #[serde(skip)]
    pub shell_measures: Vec<Value>,
    /// `pose` records keyed by node id (falling back to `source_id`), last wins.
    #[serde(skip)]
    pub node_poses: BTreeMap<String, Value>,
}

pub(crate) fn is_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-' | '@'))
}

fn str_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("`{key}` missing or not a string"))
}

fn id_field<'a>(obj: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a str, String> {
    let s = str_field(obj, key)?;
    if is_id(s) {
        Ok(s)
    } else {
        Err(format!("`{key}` is not a valid id"))
    }
}

struct Envelope<'a> {
    kind: &'a str,
    t_ns: u64,
    region: &'a str,
    proof: &'a str,
    receipt: &'a str,
}

fn check_envelope(v: &Value) -> Result<Envelope<'_>, String> {
    let obj = v.as_object().ok_or("line is not a JSON object")?;
    if str_field(obj, "schema")? != "spatial.evidence.v1" {
        return Err("schema is not spatial.evidence.v1".into());
    }
    let kind = str_field(obj, "type")?;
    let t_ns = obj
        .get("t_ns")
        .and_then(Value::as_u64)
        .ok_or("`t_ns` missing or not an unsigned integer")?;
    if str_field(obj, "frame")? != "room_enu" {
        return Err("frame is not room_enu".into());
    }
    let region = id_field(obj, "region")?;
    id_field(obj, "source_id")?;
    let u = obj
        .get("uncertainty_m")
        .and_then(Value::as_f64)
        .ok_or("`uncertainty_m` missing or not a number")?;
    if !(u.is_finite() && u > 0.0 && u <= 100.0) {
        return Err("`uncertainty_m` outside (0, 100]".into());
    }
    let prov = obj
        .get("provenance")
        .and_then(Value::as_object)
        .ok_or("`provenance` missing or not an object")?;
    let receipt = id_field(prov, "receipt").map_err(|e| format!("provenance: {e}"))?;
    id_field(prov, "producer").map_err(|e| format!("provenance: {e}"))?;
    let proof = str_field(prov, "proof").map_err(|e| format!("provenance: {e}"))?;
    if !matches!(proof, "MEASURED" | "CODE" | "SYNTHETIC") {
        return Err("provenance.proof must be MEASURED, CODE or SYNTHETIC".into());
    }
    Ok(Envelope {
        kind,
        t_ns,
        region,
        proof,
        receipt,
    })
}

/// Validate and summarise an evidence file. Aborts on the first bad line.
pub fn summarize_evidence_file(path: &Path) -> Result<EvidenceSummary, DatasetError> {
    let file = File::open(path).map_err(|e| DatasetError::io(path, e))?;
    summarize_evidence(BufReader::new(file), path)
}

/// Reader form of [`summarize_evidence_file`]; `path` is used in errors only.
/// Blank lines are skipped but still counted for line numbers. A
/// `provenance.receipt` seen twice aborts the run (ADR-111 section 1.1: unique
/// per record) and the error names both line numbers.
pub fn summarize_evidence<R: BufRead>(
    mut reader: R,
    path: &Path,
) -> Result<EvidenceSummary, DatasetError> {
    let mut buf = Vec::new();
    let mut line_no = 0usize;
    let mut sum = EvidenceSummary {
        lines: 0,
        region: String::new(),
        min_t_ns: u64::MAX,
        max_t_ns: 0,
        by_type: BTreeMap::new(),
        proof_total: ProofCounts::default(),
        unknown_types: BTreeMap::new(),
        shell_measures: Vec::new(),
        node_poses: BTreeMap::new(),
    };
    let mut regions = BTreeSet::new();
    let mut receipts: HashMap<String, usize> = HashMap::new();
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| DatasetError::io(path, e))?;
        if n == 0 {
            break;
        }
        line_no += 1;
        let bad = |reason: String| DatasetError::Evidence {
            file: path.to_path_buf(),
            line: line_no,
            reason,
        };
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        if buf.len() > MAX_LINE_BYTES {
            return Err(bad(format!(
                "line is {} bytes, limit {MAX_LINE_BYTES}",
                buf.len()
            )));
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let value: Value =
            serde_json::from_slice(&buf).map_err(|e| bad(format!("invalid JSON: {e}")))?;
        let env = check_envelope(&value).map_err(bad)?;
        if let Some(first) = receipts.insert(env.receipt.to_string(), line_no) {
            return Err(bad(format!(
                "duplicate provenance.receipt {:?}: first seen on line {first}, again on line {line_no}",
                env.receipt
            )));
        }
        sum.lines += 1;
        sum.min_t_ns = sum.min_t_ns.min(env.t_ns);
        sum.max_t_ns = sum.max_t_ns.max(env.t_ns);
        regions.insert(env.region.to_string());
        sum.proof_total.add(env.proof);
        let tc = sum.by_type.entry(env.kind.to_string()).or_default();
        tc.lines += 1;
        tc.proof.add(env.proof);
        if !KNOWN_TYPES.contains(&env.kind) {
            *sum.unknown_types.entry(env.kind.to_string()).or_default() += 1;
        }
        match env.kind {
            "shell_measure" => sum.shell_measures.push(value),
            "pose" => {
                let key = value
                    .get("node_id")
                    .or_else(|| value.get("source_id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                sum.node_poses.insert(key, value);
            }
            _ => {}
        }
    }
    if sum.lines == 0 {
        return Err(DatasetError::EmptyEvidence(path.to_path_buf()));
    }
    if regions.len() > 1 {
        return Err(DatasetError::MultipleRegions(regions.into_iter().collect()));
    }
    sum.region = regions.into_iter().next().unwrap_or_default();
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(extra: &str) -> String {
        format!(
            r#"{{"schema":"spatial.evidence.v1","type":"pose","t_ns":5,"frame":"room_enu","region":"region/urth/meso/r","source_id":"s","uncertainty_m":0.1,"provenance":{{"receipt":"r1","producer":"p@1","proof":"MEASURED"}}{extra}}}"#
        )
    }

    fn run(s: &str) -> Result<EvidenceSummary, DatasetError> {
        summarize_evidence(s.as_bytes(), Path::new("t.jsonl"))
    }

    #[test]
    fn accepts_valid_and_counts() {
        let second = line(",\"node_id\":\"n1\"").replace("\"r1\"", "\"r2\"");
        let s = run(&format!("{}\n\n{second}\n", line(",\"node_id\":\"n1\""))).unwrap();
        assert_eq!(s.lines, 2);
        assert_eq!(s.node_poses.len(), 1);
        assert_eq!(s.proof_total.measured, 2);
    }

    #[test]
    fn rejects_each_envelope_fault_with_line_number() {
        let bad = [
            line("").replace("spatial.evidence.v1", "spatial.evidence.v2"),
            line("").replace("room_enu", "world"),
            line("").replace("\"uncertainty_m\":0.1", "\"uncertainty_m\":0"),
            line("").replace("\"uncertainty_m\":0.1", "\"uncertainty_m\":100.5"),
            line("").replace("MEASURED", "CLAIMED"),
            line("").replace("\"t_ns\":5", "\"t_ns\":-5"),
            line("").replace("\"uncertainty_m\":0.1", "\"uncertainty_m\":1e999"),
        ];
        for b in bad {
            match run(&format!("{}\n{b}\n", line(""))) {
                Err(DatasetError::Evidence { line, .. }) => assert_eq!(line, 2, "{b}"),
                other => panic!("expected rejection of {b}: {other:?}"),
            }
        }
    }

    #[test]
    fn duplicate_receipt_aborts_with_both_line_numbers() {
        let other = line("").replace("\"receipt\":\"r1\"", "\"receipt\":\"r2\"");
        let text = format!("{}\n\n{other}\n{}\n", line(""), line(""));
        match run(&text) {
            Err(DatasetError::Evidence { line, reason, .. }) => {
                assert_eq!(line, 4);
                assert!(
                    reason.contains("line 1") && reason.contains("line 4"),
                    "{reason}"
                );
            }
            other => panic!("expected duplicate receipt rejection: {other:?}"),
        }
        assert!(run(&format!("{}\n{other}\n", line(""))).is_ok());
    }

    #[test]
    fn oversize_line_rejected() {
        let big = line(&format!(",\"pad\":\"{}\"", "x".repeat(MAX_LINE_BYTES)));
        assert!(matches!(
            run(&big),
            Err(DatasetError::Evidence { line: 1, .. })
        ));
    }
}
