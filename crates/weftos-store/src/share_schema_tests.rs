//! Schema 1 and 2 decode into one manifest, an rc.1 reader fails closed on schema 2, and the tree
//! digest does not depend on how the tree is stored (WO-069 lane D3, step 1).

use super::*;

fn who() -> Actor {
    Actor { agent: "a".into(), host: "h".into() }
}

fn sha(c: char) -> String {
    c.to_string().repeat(64)
}

fn manifest(schema: u32) -> TreeManifest {
    let pooled = schema >= SCHEMA_V2;
    let g = |n| pooled.then_some(n);
    TreeManifest {
        schema,
        generation: if pooled { 3 } else { 0 },
        branch: "feat/x".into(),
        base_commit: "b".repeat(40),
        head_commit: "c".repeat(40),
        files: vec![
            FileEntry { path: "docs/a.md".into(), size: 3, sha256: sha('a'), generation: g(1) },
            FileEntry { path: "src/b.rs".into(), size: 4, sha256: sha('b'), generation: g(3) },
        ],
        uncommitted: vec!["docs/a.md".into()],
        changes: vec![],
        changes_recorded: pooled,
        docs: vec![DocEntry { path: "docs/a.md".into(), title: Some("A".into()) }],
        has_bundle: true,
        bundle_sha256: Some(sha('e')),
        bundle_gen: g(2),
        retiring: if pooled { vec![Retiring { sha256: sha('d'), generation: 1, size: 9, since_ms: 5, bundle: false }] } else { vec![] },
        v1_retired_ms: None,
        writer: who(),
        published_ms: 7,
    }
}

fn bytes(m: &TreeManifest) -> Vec<u8> {
    serde_json::to_vec_pretty(m).unwrap()
}

fn parse(d: &[u8]) -> Result<TreeManifest> {
    super::super::parse_manifest(d)
}

/// The rc.1 manifest struct, copied from v0.8.5-rc.1 (`share.rs`): what an old reader does with
/// what this build writes.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct Rc1Manifest {
    schema: u32,
    branch: String,
    base_commit: String,
    head_commit: String,
    files: Vec<Rc1File>,
    uncommitted: Vec<String>,
    #[serde(default)]
    changes: Vec<PathChange>,
    docs: Vec<DocEntry>,
    has_bundle: bool,
    #[serde(default)]
    bundle_sha256: Option<String>,
    writer: Actor,
    published_ms: u64,
}
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct Rc1File {
    path: String,
    size: u64,
    sha256: String,
}

#[test]
fn both_schemas_round_trip_and_schema_2_lists_entries_not_files() {
    for s in [SCHEMA_V1, SCHEMA_V2] {
        let m = manifest(s);
        assert_eq!(parse(&bytes(&m)).unwrap(), m, "schema {s}");
    }
    let v2: serde_json::Value = serde_json::from_slice(&bytes(&manifest(SCHEMA_V2))).unwrap();
    assert!(v2.get("files").is_none() && v2["entries"].is_array(), "{v2}");
    assert_eq!(v2["objects"], LAYOUT);
    assert_eq!(v2["entries"][1]["gen"], 3);
    let v1: serde_json::Value = serde_json::from_slice(&bytes(&manifest(SCHEMA_V1))).unwrap();
    assert!(v1.get("entries").is_none() && v1.get("objects").is_none() && v1["files"][0].get("gen").is_none(), "{v1}");
}

#[test]
fn an_rc1_reader_fails_closed_on_schema_2_and_reads_this_builds_schema_1() {
    let e = serde_json::from_slice::<Rc1Manifest>(&bytes(&manifest(SCHEMA_V2))).unwrap_err().to_string();
    assert!(e.contains("missing field `files`"), "{e}");
    // The rollback writer's schema-1 output keeps extra fields an rc.1 reader ignores.
    let mut m = manifest(SCHEMA_V1);
    m.generation = 9;
    m.retiring = manifest(SCHEMA_V2).retiring;
    let old = serde_json::from_slice::<Rc1Manifest>(&bytes(&m)).unwrap();
    assert_eq!(old.files.len(), 2);
    // and this build keeps them
    let back = parse(&bytes(&m)).unwrap();
    assert_eq!((back.generation, back.retiring.len()), (9, 1));
}

#[test]
fn schema_3_an_unknown_layout_and_a_malformed_schema_2_are_refused() {
    let mut v: serde_json::Value = serde_json::from_slice(&bytes(&manifest(SCHEMA_V2))).unwrap();
    v["schema"] = 3.into();
    let e = parse(&serde_json::to_vec(&v).unwrap()).unwrap_err().to_string();
    assert!(e.contains("schema 3") && e.contains("upgrade"), "{e}");
    let edit = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut v: serde_json::Value = serde_json::from_slice(&bytes(&manifest(SCHEMA_V2))).unwrap();
        f(&mut v);
        parse(&serde_json::to_vec(&v).unwrap()).unwrap_err().to_string()
    };
    assert!(edit(&|v| v["objects"] = "sha256-flat".into()).contains("upgrade"));
    assert!(edit(&|v| { v.as_object_mut().unwrap().remove("objects"); }).contains("layout"));
    assert!(edit(&|v| { v.as_object_mut().unwrap().remove("entries"); }).contains("entries"));
    assert!(edit(&|v| v["files"] = serde_json::json!([])).contains("files"));
    assert!(edit(&|v| { v["entries"][0].as_object_mut().unwrap().remove("gen"); }).contains("docs/a.md"));
    assert!(edit(&|v| v["entries"][0]["gen"] = 4.into()).contains("docs/a.md"), "gen above the manifest generation");
    assert!(edit(&|v| v["entries"][0]["sha256"] = "../../x".into()).contains("docs/a.md"), "a sha that would escape the pool");
    assert!(edit(&|v| v["bundle_gen"] = serde_json::Value::Null).contains("bundle"));
    assert!(edit(&|v| v["retiring"][0]["sha256"] = "ab".into()).contains("retiring"));
    assert!(edit(&|v| { v.as_object_mut().unwrap().remove("generation"); }).contains("generation"));
}

#[test]
fn the_tree_digest_is_the_same_for_one_tree_stored_as_schema_1_or_2() {
    let (a, b) = (manifest(SCHEMA_V1), manifest(SCHEMA_V2));
    assert_eq!(tree_digest(&a), tree_digest(&b));
    let mut c = b.clone();
    c.files[0].generation = Some(2);
    c.retiring.clear();
    c.generation = 8;
    assert_eq!(tree_digest(&c), tree_digest(&a), "generations and retiring are storage, not content");
    c.files[0].sha256 = sha('f');
    assert_ne!(tree_digest(&c), tree_digest(&a));
}

#[test]
fn object_keys_follow_the_schema_and_parse_back() {
    let (v1, v2) = (manifest(SCHEMA_V1), manifest(SCHEMA_V2));
    assert_eq!(v1.object_key(&v1.files[0]), ".wt/tree/docs/a.md");
    assert_eq!(v1.bundle_object().as_deref(), Some(BUNDLE_KEY));
    let k = v2.object_key(&v2.files[1]);
    assert_eq!(k, format!(".wt/obj/sha256/bb/{}.g3", sha('b')));
    assert_eq!(parse_obj_key(&k), Some((sha('b'), 3)));
    assert_eq!(v2.bundle_object(), Some(obj_key(&sha('e'), 2)));
    for bad in [
        format!(".wt/obj/sha256/ab/{}.g3", sha('b')), // fan does not match
        format!(".wt/obj/sha256/bb/{}.g03", sha('b')),
        format!(".wt/obj/sha256/bb/{}.g0", sha('b')),
        format!(".wt/obj/sha256/bb/{}", sha('b')),
        ".wt/obj/sha256/bb/bb.g1".to_string(),
        format!(".wt/tree/{}", sha('b')),
    ] {
        assert_eq!(parse_obj_key(&bad), None, "{bad}");
    }
}

#[test]
fn a_schema_1_manifest_from_before_the_flag_counts_a_listed_change_as_recorded() {
    let mut v: serde_json::Value = serde_json::from_slice(&bytes(&manifest(SCHEMA_V1))).unwrap();
    assert!(v.get("changes_recorded").is_none());
    assert!(!parse(&serde_json::to_vec(&v).unwrap()).unwrap().changes_recorded);
    v["changes"] = serde_json::json!([{"path": "src/b.rs", "kind": "modified"}]);
    assert!(parse(&serde_json::to_vec(&v).unwrap()).unwrap().changes_recorded);
    // schema 2 always says, including "recorded and empty"
    let v2: serde_json::Value = serde_json::from_slice(&bytes(&manifest(SCHEMA_V2))).unwrap();
    assert_eq!(v2["changes_recorded"], true);
}

#[test]
fn the_retiring_list_is_capped_at_twice_the_file_ceiling() {
    let lim = Limits { max_files: 2, ..Limits::default() };
    let mut m = manifest(SCHEMA_V2);
    m.files.truncate(1);
    m.retiring = (0..5).map(|i| Retiring { sha256: sha('d'), generation: i + 1, size: 1, since_ms: 0, bundle: false }).collect();
    let e = super::super::parse_manifest_with(&bytes(&m), &lim).unwrap_err().to_string();
    assert!(e.contains("retires more than the 4"), "{e}");
    m.retiring.truncate(4);
    assert!(super::super::parse_manifest_with(&bytes(&m), &lim).is_ok());
}
