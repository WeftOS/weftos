use super::*;

fn dg(n: u8) -> String {
    format!("sha256:{:02x}{}", n, "0".repeat(62))
}

fn rec(object: &str, version: &str, n: u8, parent: Option<u8>, deps: Vec<DepRef>) -> VersionRecord {
    VersionRecord {
        object: object.into(),
        version: version.into(),
        digest: dg(n),
        parent: parent.map(dg),
        dependencies: deps,
        recorded_at: 1_000 + u64::from(n),
    }
}

fn dep(name: &str, n: Option<u8>) -> DepRef {
    DepRef { kind: "agent".into(), name: name.into(), digest: n.map(dg) }
}

fn idx() -> (tempfile::TempDir, VersionsIndex) {
    let d = tempfile::tempdir().unwrap();
    let i = VersionsIndex::new(d.path().join("versions"));
    (d, i)
}

#[test]
fn record_appends_one_line_and_is_idempotent() {
    let (_d, i) = idx();
    let r = rec("steward", "0.8.4", 1, None, vec![]);
    assert_eq!(i.record("agents", &r).unwrap(), Recorded::Appended);
    assert_eq!(i.record("agents", &r).unwrap(), Recorded::AlreadyRecorded);
    assert_eq!(i.list("agents").unwrap(), vec![r]);
    let text = std::fs::read_to_string(i.dir().join("agents.jsonl")).unwrap();
    assert_eq!(text.lines().count(), 1);
}

#[test]
fn refuses_bad_input_rather_than_returning_empty() {
    let (_d, i) = idx();
    let mut bad = rec("steward", "0.8.4", 1, None, vec![]);
    bad.digest = "sha256:abc".into();
    assert!(matches!(i.record("agents", &bad), Err(VersionsError::Invalid { what: "digest", .. })));
    assert!(matches!(i.record("agents", &rec("../x", "1", 1, None, vec![])), Err(VersionsError::Invalid { .. })));
    assert!(matches!(i.record("Agents", &rec("a", "1", 1, None, vec![])), Err(VersionsError::Invalid { what: "kind", .. })));
    // Unknown parent and conflicting version are refusals, and nothing is written.
    assert!(matches!(i.record("agents", &rec("a", "1", 2, Some(9), vec![])), Err(VersionsError::UnknownParent { .. })));
    i.record("agents", &rec("a", "1", 1, None, vec![])).unwrap();
    assert!(matches!(i.record("agents", &rec("a", "2", 1, None, vec![])), Err(VersionsError::Conflict { .. })));
    assert_eq!(i.list("agents").unwrap().len(), 1);
    // A never-recorded object is NotFound, not an empty history.
    assert!(matches!(i.history("agents", "nobody"), Err(VersionsError::NotFound { .. })));
    assert!(matches!(i.tree("agents", "nobody"), Err(VersionsError::NotFound { .. })));
    assert!(matches!(i.history("agents", "bad name"), Err(VersionsError::Invalid { .. })));
}

#[test]
fn corrupt_line_is_refused_with_its_line_number() {
    let (_d, i) = idx();
    i.record("agents", &rec("a", "1", 1, None, vec![])).unwrap();
    let path = i.dir().join("agents.jsonl");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{not json}\n");
    std::fs::write(&path, text).unwrap();
    match i.list("agents") {
        Err(VersionsError::Corrupt { line, .. }) => assert_eq!(line, 2),
        other => panic!("expected Corrupt, got {other:?}"),
    }
    assert!(i.history("agents", "a").is_err());
}

#[test]
fn history_tree_and_sub_packages() {
    let (_d, i) = idx();
    i.record("agents", &rec("domain-expert", "1.0", 1, None, vec![])).unwrap();
    i.record("agents", &rec("domain-expert", "1.1", 2, Some(1), vec![])).unwrap();
    // A derived package of another object, parented on 1.1.
    i.record("agents", &rec("acme-expert", "0.1", 3, Some(2), vec![])).unwrap();
    let h = i.history("agents", "domain-expert").unwrap();
    assert_eq!(h.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(), ["1.0", "1.1"]);
    let t = i.tree("agents", "domain-expert").unwrap();
    assert_eq!(t.len(), 1);
    assert_eq!(t[0].record.version, "1.0");
    assert_eq!(t[0].children[0].record.version, "1.1");
    assert_eq!(t[0].children[0].children[0].record.object, "acme-expert");
    // The derived object's own tree is rooted at its version, whose parent is foreign.
    let t2 = i.tree("agents", "acme-expert").unwrap();
    assert_eq!(t2[0].record.version, "0.1");
}

#[test]
fn deps_forward_and_reverse() {
    let (_d, i) = idx();
    i.record("agents", &rec("steward", "1", 1, None, vec![])).unwrap();
    i.record("agents", &rec("chief-of-staff", "1", 2, None, vec![dep("steward", Some(1))])).unwrap();
    i.record("teams", &rec("core", "1", 3, None, vec![dep("steward", Some(1)), dep("chief-of-staff", Some(2))])).unwrap();
    let v = i.deps("agents", "steward").unwrap();
    assert!(v.depends_on.is_empty());
    let mut who: Vec<_> = v.dependents.iter().map(|d| format!("{}/{}", d.kind, d.object)).collect();
    who.sort();
    assert_eq!(who, ["agents/chief-of-staff", "teams/core"]);
    let v = i.deps("agents", "chief-of-staff").unwrap();
    assert_eq!(v.depends_on[0].name, "steward");
    assert_eq!(v.dependents.len(), 1);
}

#[test]
fn only_latest_versions_count_as_dependents() {
    let (_d, i) = idx();
    i.record("agents", &rec("steward", "1", 1, None, vec![])).unwrap();
    i.record("agents", &rec("cos", "1", 2, None, vec![dep("steward", Some(1))])).unwrap();
    i.record("agents", &rec("cos", "2", 3, Some(2), vec![])).unwrap();
    assert!(i.deps("agents", "steward").unwrap().dependents.is_empty());
}

#[test]
fn mirror_keeps_one_lineage_per_object_with_a_vector_per_version() {
    let (_d, i) = idx();
    i.record("agents", &rec("domain-expert", "1.0", 1, None, vec![])).unwrap();
    i.record("agents", &rec("domain-expert", "1.1", 2, Some(1), vec![])).unwrap();
    assert!(i.has_lineage("agents", "domain-expert"));
    assert!(!i.has_lineage("agents", "acme-expert"));
    // A derivation gets its own lineage, and its later versions still mirror.
    i.record("agents", &rec("acme-expert", "0.1", 3, Some(2), vec![])).unwrap();
    i.record("agents", &rec("acme-expert", "0.2", 4, Some(3), vec![])).unwrap();
    let parent = i.open_lineage("agents", "domain-expert").unwrap();
    let child = i.open_lineage("agents", "acme-expert").unwrap();
    assert_eq!(parent.status().base.total_vectors, 2);
    assert_eq!(child.status().base.total_vectors, 2);
}

#[test]
fn reopened_index_sees_earlier_records() {
    let (d, i) = idx();
    i.record("cogs", &rec("ecg", "1", 1, None, vec![])).unwrap();
    let again = VersionsIndex::new(d.path().join("versions"));
    assert_eq!(again.kinds().unwrap(), ["cogs"]);
    assert_eq!(again.history("cogs", "ecg").unwrap().len(), 1);
}

fn catalog_entry(id: &str, n: u8, parent: Option<u8>) -> serde_json::Value {
    let mut deps = vec![serde_json::json!({ "kind": "skill", "name": "s" })];
    if id != "base" {
        deps.push(serde_json::json!({ "kind": "agent", "name": "base", "digest": dg(1) }));
    }
    serde_json::json!({ "id": id, "version": "0.8.4", "digest": dg(n), "parent": parent.map(dg), "dependencies": deps })
}

#[test]
fn import_orders_parents_first_extends_history_and_is_idempotent() {
    let (_d, i) = idx();
    // Child listed before its parent.
    let cat = serde_json::json!({ "agents": [catalog_entry("child", 2, Some(1)), catalog_entry("base", 1, None)] });
    let r = i.import_catalog("agents", &cat, 5).unwrap();
    assert_eq!((r.appended, r.already_recorded), (2, 0));
    let again = i.import_catalog("agents", &cat, 6).unwrap();
    assert_eq!((again.appended, again.already_recorded), (0, 2));
    // base changed: its new version takes the old one as parent.
    let cat2 = serde_json::json!({ "agents": [catalog_entry("base", 3, None)] });
    i.import_catalog("agents", &cat2, 7).unwrap();
    let h = i.history("agents", "base").unwrap();
    assert_eq!(h[1].parent.as_deref(), Some(dg(1).as_str()));
    assert_eq!(i.deps("agents", "base").unwrap().dependents.len(), 1);
}

#[test]
fn import_refuses_unknown_parent_and_malformed_catalog() {
    let (_d, i) = idx();
    let cat = serde_json::json!({ "agents": [catalog_entry("orphan", 2, Some(9))] });
    assert!(matches!(i.import_catalog("agents", &cat, 1), Err(VersionsError::UnknownParent { .. })));
    assert!(matches!(i.import_catalog("agents", &serde_json::json!({}), 1), Err(VersionsError::Invalid { .. })));
    let no_digest = serde_json::json!({ "agents": [{ "id": "x", "version": "1" }] });
    assert!(matches!(i.import_catalog("agents", &no_digest, 1), Err(VersionsError::Invalid { .. })));
    assert!(i.list("agents").unwrap().is_empty());
}
