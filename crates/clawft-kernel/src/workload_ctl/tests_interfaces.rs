//! `[cog].interfaces` become placement requirements (cog plan P1, part C).

use std::collections::BTreeMap;
use std::sync::Arc;

use super::cog_workload_spec;
use super::test_support::*;
use crate::workload_runtime::cog_spec::CogSpec;
use crate::workload_runtime::types::{Binary, SignedPayload};
use crate::workload_runtime::{VerifiedWorkload, WorkloadSource};
use clawft_types::placement::Capability;

fn workload(toml_extra: &str) -> VerifiedWorkload {
    let toml = format!("[cog]\nid = \"probe\"\nversion = \"0.1.0\"\n{toml_extra}");
    let spec = CogSpec::parse(toml.as_bytes()).unwrap();
    let mut binaries = BTreeMap::new();
    binaries.insert(
        arch().to_string(),
        Binary {
            blake3: "00".repeat(32),
            bytes: Arc::new(b"#!/bin/sh\n".to_vec()),
        },
    );
    VerifiedWorkload {
        kind: "cog".into(),
        id: "probe".into(),
        version: "0.1.0".into(),
        source: WorkloadSource::SignedPackage(Box::new(SignedPayload {
            package_id: "pkg".into(),
            signer_keys: vec![],
            spec,
            binaries,
            artifact_hashes: vec![],
        })),
    }
}

fn common_match(toml_extra: &str, caps: &[Capability]) -> bool {
    let spec = cog_workload_spec(&workload(toml_extra)).unwrap();
    spec.requirements.common.iter().all(|r| r.matches(caps))
}

#[test]
fn i2c_cog_matches_only_a_node_with_the_bus() {
    let mut with = board_caps("pi");
    with.push(cap("bus.i2c"));
    let without = board_caps("pi");
    let toml = "interfaces = [\"i2c\"]\n";
    assert!(common_match(toml, &with));
    assert!(!common_match(toml, &without));
}

#[test]
fn prefix_interfaces_match_subfamilies() {
    let caps = vec![cap("media.camera.usb"), cap("accel.npu.qualcomm"), cap("sdr.iio")];
    let toml = "interfaces = [\"camera\", \"npu\", \"sdr\"]\n";
    assert!(common_match(toml, &caps));
    assert!(!common_match(toml, &caps[..2]));
    assert!(!common_match("interfaces = [\"audio-in\"]\n", &caps));
}

#[test]
fn no_interfaces_or_none_leave_common_empty() {
    for extra in ["", "interfaces = []\n", "interfaces = [\"none\"]\n"] {
        let spec = cog_workload_spec(&workload(extra)).unwrap();
        assert!(spec.requirements.common.is_empty(), "{extra:?}");
    }
    let spec = cog_workload_spec(&workload("hardware_requirement = [\"pi-zero-2w\"]\n")).unwrap();
    assert!(spec.requirements.common.is_empty());
}

#[test]
fn each_interface_adds_one_requirement() {
    let spec = cog_workload_spec(&workload("interfaces = [\"i2c\", \"spi\", \"camera\"]\n")).unwrap();
    assert_eq!(spec.requirements.common.len(), 3);
}
