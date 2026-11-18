//! Hostile-names fetch (WO-069 gate item 2, ADR-131 winpath rules): a tampered or foreign manifest
//! lists names Windows treats specially. `fetch_tree` must refuse them (on every platform) and write
//! the rest. Runs against the in-process `MemBackend`; the files are written to the real local
//! filesystem under a temp dir, which is what differs on Windows. No network, no bucket.
use weftos_store::overlay::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const WT: &str = "w-01abc-aaaaaaaaaaaa";

fn plant(b: &MemBackend, names: &[String]) {
    let files = names
        .iter()
        .map(|n| FileEntry { path: n.clone(), size: 1, sha256: sha256_hex(b"x"), generation: None })
        .collect();
    let m = TreeManifest {
        schema: 1,
        generation: 0,
        branch: "f".into(),
        base_commit: "a".into(),
        head_commit: "b".into(),
        files,
        uncommitted: vec![],
        changes: vec![],
        docs: vec![],
        has_bundle: false,
        bundle_sha256: None,
        changes_recorded: false,
        bundle_gen: None,
        retiring: vec![],
        v1_retired_ms: None,
        writer: Actor { agent: "w".into(), host: "h".into() },
        published_ms: 1,
    };
    b.put(WT, MANIFEST_KEY, &serde_json::to_vec(&m).unwrap()).unwrap();
    for n in names {
        // the object may be un-storable under a hostile key on some backends; the manifest is what lies
        let _ = b.put(WT, &format!("{TREE_PREFIX}{n}"), b"x");
    }
}

#[test]
fn hostile_names_on_fetch_report() {
    let hostile: Vec<String> = [
        "docs/CON", "docs/con.txt", "NUL.md", "a/COM1", "a/lpt9.log", "a/AUX.tar.gz", "a/CONOUT$",
        "a/trailing.", "a/trailing ", "a/b?c", "a/b*c", "a/b|c", "a/b\"c", "a/b<c", "a/b>c", "a/b:c",
        "a/GIT~1/config", "a/ctl\u{1}x",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let ok: Vec<String> = ["docs/console.md", "a/com0", "src/lib.rs", "docs/a.b.md"].iter().map(|s| s.to_string()).collect();
    let b = MemBackend::new();
    let mut all = hostile.clone();
    all.extend(ok.clone());
    plant(&b, &all);
    let dest = tempfile::tempdir().unwrap();
    let r = fetch_tree(&b, WT, dest.path(), Select::All, true).unwrap();
    eprintln!("HOSTILE written={:?}\nHOSTILE refused={:?}", r.written, r.refused);
    for h in &hostile {
        assert!(r.refused.contains(h), "hostile name not refused: {h:?}");
        assert!(!r.written.contains(h), "hostile name written: {h:?}");
    }
    for o in &ok {
        assert!(r.written.contains(o), "safe name not written: {o:?}");
        assert!(dest.path().join(o).is_file(), "safe name missing on disk: {o:?}");
    }
}

#[test]
fn case_collision_and_long_paths_report() {
    let b = MemBackend::new();
    let seg200 = "s".repeat(196); // + ".txt" = 200 bytes, the store cap
    let long_total = format!("{}/{}/leaf.txt", "d".repeat(120), "e".repeat(120)); // path under dest > 259 on Windows
    let over_seg = "t".repeat(201);
    let names = vec![
        "Readme.md".to_string(),
        "README.MD".to_string(),
        "dir/Case.txt".to_string(),
        "DIR/case.TXT".to_string(),
        long_total.clone(),
        format!("{seg200}.txt"),
        format!("{over_seg}.txt"),
    ];
    plant(&b, &names);
    let dest = tempfile::tempdir().unwrap();
    let res = fetch_tree(&b, WT, dest.path(), Select::All, true);
    match &res {
        Ok(r) => eprintln!("CASELONG written={:?}\nCASELONG refused={:?}", r.written, r.refused),
        Err(e) => eprintln!("CASELONG error={e}"),
    }
    let r = res.unwrap();
    // exactly one of each case-colliding pair lands; the later one is refused
    assert!(r.written.contains(&"Readme.md".to_string()) && r.refused.contains(&"README.MD".to_string()));
    assert!(r.written.contains(&"dir/Case.txt".to_string()) && r.refused.contains(&"DIR/case.TXT".to_string()));
    assert!(r.written.contains(&format!("{seg200}.txt")));
    // a segment over the store's 200-byte cap is refused
    assert!(r.refused.contains(&format!("{over_seg}.txt")));
    let long_full = dest.path().join(&long_total);
    eprintln!("CASELONG long_total full_len={} written={} on_disk={}", long_full.as_os_str().len(), r.written.contains(&long_total), long_full.is_file());
}
