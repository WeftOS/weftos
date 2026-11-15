//! Minimal SenML pack validation (ADR-111 section 2). Checks the shape and
//! the proof tag only. Quantity names, units and ranges are the vocabulary
//! file's job and are not checked here.

use crate::error::DatasetError;
use crate::evidence::{MAX_LINE_BYTES, is_id};
use crate::proof::ProofCounts;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub const MAX_RECORDS_PER_PACK: usize = 256;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadingsSummary {
    pub packs: u64,
    pub records: u64,
    pub proof: ProofCounts,
    /// Span of `(bt + t)` in nanoseconds when every record resolved a time.
    pub min_t_ns: Option<u64>,
    pub max_t_ns: Option<u64>,
}

pub fn summarize_readings_file(path: &Path) -> Result<ReadingsSummary, DatasetError> {
    let file = File::open(path).map_err(|e| DatasetError::io(path, e))?;
    summarize_readings(BufReader::new(file), path)
}

/// Validate each non-blank line as one SenML pack. Aborts on the first bad line.
pub fn summarize_readings<R: BufRead>(
    mut reader: R,
    path: &Path,
) -> Result<ReadingsSummary, DatasetError> {
    let mut sum = ReadingsSummary {
        packs: 0,
        records: 0,
        proof: ProofCounts::default(),
        min_t_ns: None,
        max_t_ns: None,
    };
    let mut buf = Vec::new();
    let mut line_no = 0usize;
    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| DatasetError::io(path, e))?;
        if n == 0 {
            break;
        }
        line_no += 1;
        let bad = |reason: String| DatasetError::Readings {
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
        check_pack(&value, &mut sum).map_err(bad)?;
        sum.packs += 1;
    }
    Ok(sum)
}

fn check_pack(v: &Value, sum: &mut ReadingsSummary) -> Result<(), String> {
    let pack = v.as_array().ok_or("line is not a JSON array")?;
    if pack.is_empty() || pack.len() > MAX_RECORDS_PER_PACK {
        return Err(format!(
            "pack has {} records, need 1 to {MAX_RECORDS_PER_PACK}",
            pack.len()
        ));
    }
    let mut bn_seen = false;
    let mut bt = 0.0f64;
    for (i, rec) in pack.iter().enumerate() {
        let obj = rec
            .as_object()
            .ok_or_else(|| format!("record {i} is not an object"))?;
        if let Some(b) = obj.get("bn") {
            let b = b
                .as_str()
                .ok_or_else(|| format!("record {i}: `bn` is not a string"))?;
            if !b.strip_suffix('/').is_some_and(is_id) {
                return Err(format!(
                    "record {i}: `bn` must be a source id ending in `/`"
                ));
            }
            bn_seen = true;
        }
        if let Some(k) = obj.keys().find(|k| k.ends_with('_') && *k != "proof_") {
            return Err(format!("record {i}: unknown must-understand field `{k}`"));
        }
        if !bn_seen {
            return Err(format!("record {i}: no `bn` in scope"));
        }
        let proof = obj
            .get("proof_")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("record {i}: `proof_` missing or not a string"))?;
        if !sum.proof.add(proof) {
            return Err(format!(
                "record {i}: `proof_` must be MEASURED, CODE or SYNTHETIC"
            ));
        }
        sum.records += 1;
        if let Some(b) = obj.get("bt").and_then(Value::as_f64) {
            bt = b;
        }
        let t = obj.get("t").and_then(Value::as_f64).unwrap_or(0.0);
        let abs = (bt + t) * 1e9;
        if abs.is_finite() && abs >= 0.0 {
            let ns = abs as u64;
            sum.min_t_ns = Some(sum.min_t_ns.map_or(ns, |m| m.min(ns)));
            sum.max_t_ns = Some(sum.max_t_ns.map_or(ns, |m| m.max(ns)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &str) -> Result<ReadingsSummary, DatasetError> {
        summarize_readings(s.as_bytes(), Path::new("r.jsonl"))
    }

    #[test]
    fn counts_and_carries_bn() {
        let s = run(r#"[{"n":"temperature","v":1,"proof_":"MEASURED","bn":"a/","bt":1759500000},{"n":"x","v":1,"proof_":"CODE"}]"#).unwrap();
        assert_eq!(
            (s.packs, s.records, s.proof.measured, s.proof.code),
            (1, 2, 1, 1)
        );
        assert_eq!(s.min_t_ns, Some(1_759_500_000_000_000_000));
    }

    #[test]
    fn rejections() {
        for bad in [
            r#"{"n":"x"}"#,
            r#"[]"#,
            r#"[{"n":"x","v":1,"proof_":"MEASURED"}]"#,
            r#"[{"n":"x","v":1,"proof_":"MEASURED","bn":"a"}]"#,
            r#"[{"n":"x","v":1,"bn":"a/"}]"#,
            r#"[{"n":"x","v":1,"proof_":"CLAIMED","bn":"a/"}]"#,
            r#"[{"n":"x","v":1,"proof_":"MEASURED","bn":"a b/"}]"#,
            r#"[{"n":"x","v":1,"proof_":"MEASURED","bn":"a/","future_":1}]"#,
        ] {
            assert!(
                matches!(run(bad), Err(DatasetError::Readings { line: 1, .. })),
                "{bad}"
            );
        }
    }
}
