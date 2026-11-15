use super::*;
use std::io::Write;

fn session() -> (tempfile::TempDir, ArchiveRequest, tempfile::TempDir) {
    let src = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session");
    for e in fs::read_dir(fixtures).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), src.path().join(e.file_name())).unwrap();
    }
    let dest = tempfile::tempdir().unwrap();
    let mut req = ArchiveRequest::new(src.path(), dest.path());
    req.consent = Some(serde_json::json!({"given": true}));
    (src, req, dest)
}

fn csi(src: &Path) -> PathBuf {
    src.join("demo-session-001.csi.jsonl")
}

#[test]
fn source_growing_between_hash_and_copy_is_refused() {
    let (src, req, dest) = session();
    let mut grow = |_: &[PathBuf]| {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(csi(src.path()))
            .unwrap();
        f.write_all(b"more").unwrap();
    };
    match archive_inner(&req, &mut grow) {
        Err(DatasetError::SourceChanged(p)) => assert_eq!(p, csi(src.path())),
        other => panic!("expected SourceChanged: {other:?}"),
    }
    // Nothing published, no temp directory left.
    let datasets = dest.path().join("ruview-demo/datasets");
    assert!(!datasets.join("demo-session-001").exists());
    assert_eq!(fs::read_dir(datasets).unwrap().count(), 0);
}

#[test]
fn mtime_only_change_is_refused() {
    let (src, req, _dest) = session();
    let mut touch = |_: &[PathBuf]| {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(csi(src.path()))
            .unwrap();
        f.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
    };
    assert!(matches!(
        archive_inner(&req, &mut touch),
        Err(DatasetError::SourceChanged(_))
    ));
}

#[test]
fn untouched_sources_pass_the_stability_check() {
    let (_src, req, _dest) = session();
    assert!(archive_inner(&req, &mut |_| {}).unwrap().verification.ok);
}
