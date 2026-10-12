use super::*;
use clawft_versions::{DepRef, VersionRecord};

fn dg(n: u8) -> String {
    format!("sha256:{n:02x}{}", "0".repeat(62))
}

fn rec(object: &str, n: u8, parent: Option<u8>, deps: Vec<DepRef>) -> VersionRecord {
    VersionRecord {
        object: object.into(),
        version: "0.8.4".into(),
        digest: dg(n),
        parent: parent.map(dg),
        dependencies: deps,
        recorded_at: 1,
    }
}

fn seeded() -> (tempfile::TempDir, std::path::PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("versions");
    let i = VersionsIndex::new(&dir);
    i.record("agents", &rec("domain-expert", 1, None, vec![])).unwrap();
    i.record("agents", &rec("acme-expert", 2, Some(1), vec![DepRef { kind: "agent".into(), name: "domain-expert".into(), digest: Some(dg(1)) }])).unwrap();
    (d, dir)
}

#[test]
fn list_tree_and_deps_answer_from_the_files() {
    let (_d, dir) = seeded();
    let v = query(&dir, "versions.list", &json!({})).unwrap();
    assert_eq!(v["versions"].as_array().unwrap().len(), 2);
    let t = query(&dir, "versions.tree", &json!({"name": "domain-expert"})).unwrap();
    assert_eq!(t["tree"][0]["children"][0]["record"]["object"], "acme-expert");
    let d = query(&dir, "versions.deps", &json!({"name": "domain-expert"})).unwrap();
    assert_eq!(d["deps"]["dependents"][0]["object"], "acme-expert");
}

#[test]
fn refusals_are_errors_not_empty_results() {
    let (_d, dir) = seeded();
    assert!(query(&dir, "versions.tree", &json!({})).unwrap_err().contains("name is required"));
    assert!(query(&dir, "versions.tree", &json!({"name": "nobody"})).unwrap_err().contains("no versions recorded"));
    assert!(query(&dir, "versions.list", &json!({"kind": "../x"})).unwrap_err().contains("invalid kind"));
    assert!(query(&dir, "versions.list", &json!({"kind": 5})).unwrap_err().contains("must be a string"));
    assert!(query(&dir, "versions.record", &json!({})).unwrap_err().contains("unknown method"));
    // A kind with no file lists as empty, which is exactly "nothing recorded".
    assert_eq!(query(&dir, "versions.list", &json!({"kind": "cogs"})).unwrap()["versions"], json!([]));
}

#[test]
fn versions_methods_are_read_and_unlisted_versions_verbs_are_admin() {
    use crate::capability::{Capability, required_capability};
    for m in ["versions.list", "versions.tree", "versions.deps"] {
        assert_eq!(required_capability(m), Capability::Read, "{m}");
    }
    assert_eq!(required_capability("versions.record"), Capability::Admin);
}
