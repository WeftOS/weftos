//! Conformance of the archiver's envelope and pack checks against the ADR-111
//! vectors in `contracts/sensors/vectors`.
//!
//! The archiver checks the evidence envelope (ADR-111 section 1.1) and a
//! minimal SenML pack shape. Per-type geometry and the readings vocabulary
//! belong to other validators, so those vectors are reported as out of scope.
//! The test fails only on a wrong verdict.

use serde_json::Value;
use std::path::{Path, PathBuf};
use weftos_worldmodel_dataset::evidence::summarize_evidence_file;
use weftos_worldmodel_dataset::senml::summarize_readings_file;

fn vectors_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/sensors/vectors")
}

/// Invalid evidence vectors whose offending field is an envelope field.
/// Classes `version` and `size` are always envelope rules. For `text`,
/// `shape` and `range` the `env_` file prefix marks an envelope field, except
/// the coordinate bound, which is a per-type field (`min`/`max`).
fn evidence_envelope_owned(class: &str, file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    match class {
        "version" | "size" => true,
        "text" | "shape" | "range" => name.starts_with("env_") && name != "env_coord_too_far.jsonl",
        _ => false,
    }
}

/// Invalid SenML vectors the minimal pack check is responsible for.
const SENML_OWNED: [&str; 12] = [
    "not_json",
    "not_an_array",
    "empty_pack",
    "too_many_records",
    "line_too_long",
    "missing_bn",
    "bn_no_slash",
    "bn_bad_chars",
    "missing_proof",
    "proof_bad_value",
    "proof_lowercase",
    "unknown_must_understand",
];

#[derive(Default)]
struct Tally {
    accepted: u32,
    rejected: u32,
    out_of_scope: u32,
    failures: Vec<String>,
}

impl Tally {
    fn row(&self, label: &str) -> String {
        format!(
            "{label:<17} accepted {:>3}  rejected {:>3}  out of scope {:>3}",
            self.accepted, self.rejected, self.out_of_scope
        )
    }
}

#[test]
fn archiver_checks_agree_with_the_adr_111_vectors() {
    let dir = vectors_dir();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let cases = manifest["cases"].as_array().expect("cases array");
    assert!(cases.len() > 100, "manifest looks truncated");

    let (mut ev_valid, mut ev_invalid) = (Tally::default(), Tally::default());
    let (mut sm_valid, mut sm_invalid) = (Tally::default(), Tally::default());

    for case in cases {
        let file = case["file"].as_str().unwrap();
        let kind = case["kind"].as_str().unwrap();
        let expect = case["expect"].as_str().unwrap();
        let class = case["class"].as_str().unwrap_or("");
        let path = dir.join(file);
        assert!(path.is_file(), "manifest names a missing file: {file}");
        let stem = file.rsplit('/').next().unwrap().trim_end_matches(".jsonl");

        match (kind, expect) {
            ("evidence", "valid") => {
                let r = summarize_evidence_file(&path);
                match r {
                    Ok(_) => ev_valid.accepted += 1,
                    Err(e) => ev_valid.failures.push(format!("{file}: rejected: {e}")),
                }
            }
            ("evidence", "invalid") if class == "unknown_type" => {
                // Section 1.2: an unknown type is not fatal to the archive, it is counted.
                match summarize_evidence_file(&path) {
                    Ok(s) if !s.unknown_types.is_empty() => ev_invalid.out_of_scope += 1,
                    other => ev_invalid
                        .failures
                        .push(format!("{file}: expected it counted as unknown: {other:?}")),
                }
            }
            ("evidence", "invalid") if evidence_envelope_owned(class, file) => {
                match summarize_evidence_file(&path) {
                    Err(_) => ev_invalid.rejected += 1,
                    Ok(_) => ev_invalid.failures.push(format!(
                        "{file}: [{class}] accepted but is an envelope fault"
                    )),
                }
            }
            ("evidence", "invalid") => match summarize_evidence_file(&path) {
                Ok(_) => ev_invalid.out_of_scope += 1,
                Err(e) => ev_invalid.failures.push(format!(
                    "{file}: [{class}] per-type fault rejected by the envelope check: {e}"
                )),
            },
            ("senml", "valid") => match summarize_readings_file(&path) {
                Ok(_) => sm_valid.accepted += 1,
                Err(e) => sm_valid.failures.push(format!("{file}: rejected: {e}")),
            },
            ("senml", "invalid") if SENML_OWNED.contains(&stem) => {
                match summarize_readings_file(&path) {
                    Err(_) => sm_invalid.rejected += 1,
                    Ok(_) => sm_invalid
                        .failures
                        .push(format!("{file}: accepted but is a pack-shape fault")),
                }
            }
            ("senml", "invalid") => sm_invalid.out_of_scope += 1,
            other => panic!("unhandled manifest case {other:?} for {file}"),
        }
    }

    println!(
        "contracts conformance (vectors_version {})",
        manifest["vectors_version"]
    );
    println!("  {}", ev_valid.row("evidence/valid"));
    println!("  {}", ev_invalid.row("evidence/invalid"));
    println!("  {}", sm_valid.row("senml/valid"));
    println!("  {}", sm_invalid.row("senml/invalid"));

    let failures: Vec<&String> = [&ev_valid, &ev_invalid, &sm_valid, &sm_invalid]
        .into_iter()
        .flat_map(|t| &t.failures)
        .collect();
    assert!(failures.is_empty(), "wrong verdicts:\n{}", {
        failures
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    });
}

#[test]
fn every_vector_file_is_listed_in_the_manifest() {
    let dir = vectors_dir();
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    let listed: std::collections::BTreeSet<String> = manifest["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["file"].as_str().unwrap().to_string())
        .collect();
    for sub in [
        "evidence/valid",
        "evidence/invalid",
        "senml/valid",
        "senml/invalid",
    ] {
        for e in std::fs::read_dir(dir.join(sub)).unwrap() {
            let name = format!("{sub}/{}", e.unwrap().file_name().to_string_lossy());
            assert!(listed.contains(&name), "vector not in manifest: {name}");
        }
    }
}
