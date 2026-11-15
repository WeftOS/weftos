//! Store upload step, exercised with a fake `weaver` that logs its argv.
#![cfg(unix)]

use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use weftos_worldmodel_dataset::{
    ArchiveRequest, ObjectStatus, StoreTarget, archive_session, verify_archive,
};

const SESSION: &str = "demo-session-001";

fn session_copy() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session");
    for e in fs::read_dir(fixtures).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), td.path().join(e.file_name())).unwrap();
    }
    td
}

/// Fake weaver: logs one tab-joined argv line per call and exits 1 when the
/// key (fourth argument) contains `fail`.
fn fake_weaver(dir: &Path) -> (String, PathBuf) {
    let log = dir.join("argv.log");
    let bin = dir.join("weaver");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\t%s\\t%s\\t%s\\t%s\\t%s\\t%s\\t%s\\n' \"$@\" >> '{}'\ncase \"$4\" in *fail*) echo 'denied by grant' >&2; exit 1;; esac\nexit 0\n",
        log.display()
    );
    fs::write(&bin, script).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    (bin.display().to_string(), log)
}

fn request(src: &Path, dest: &Path, target: StoreTarget) -> ArchiveRequest {
    let mut r = ArchiveRequest::new(src, dest);
    r.consent = Some(json!({"given": true}));
    r.store = Some(target);
    r
}

fn argv(log: &Path) -> Vec<Vec<String>> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|l| l.split('\t').map(String::from).collect())
        .collect()
}

fn read_manifest(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn uploads_each_file_then_manifest_with_expected_argv() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let (bin, log) = fake_weaver(tools.path());
    let mut t = StoreTarget::new("lab", "datasets/", "local:cog:archiver");
    t.weaver_bin = Some(bin);
    let out = archive_session(&request(src.path(), dest.path(), t)).unwrap();

    let up = out.store_upload.as_ref().unwrap();
    assert!(up.ok);
    let calls = argv(&log);
    assert_eq!(calls.len(), out.manifest.files.len() + 1);
    let names = out
        .manifest
        .files
        .iter()
        .map(|f| f.name.as_str())
        .chain(["manifest.json"]);
    for (call, name) in calls.iter().zip(names) {
        assert_eq!(call[0..2], ["store", "put"]);
        assert_eq!(call[2], "lab");
        assert_eq!(call[3], format!("datasets/{SESSION}/{name}"));
        assert_eq!(call[4], "--as");
        assert_eq!(call[5], "local:cog:archiver");
        assert_eq!(call[6], "--file");
        assert_eq!(call[7], out.dest_dir.join(name).display().to_string());
    }
    assert!(up.objects.iter().all(|o| o.status == ObjectStatus::Ok));
}

#[test]
fn manifest_copy_gains_section_and_checksums_still_verify() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let (bin, _log) = fake_weaver(tools.path());
    let mut t = StoreTarget::new("lab", "", "weftos:w1");
    t.weaver_bin = Some(bin);
    let out = archive_session(&request(src.path(), dest.path(), t)).unwrap();

    let m = read_manifest(&out.dest_dir);
    let sec = &m["store_upload"];
    assert_eq!(sec["store_name"], "lab");
    assert_eq!(sec["ok"], true);
    assert_eq!(
        sec["objects"].as_array().unwrap().len(),
        out.manifest.files.len()
    );
    assert!(
        sec["objects"][0]["key"]
            .as_str()
            .unwrap()
            .starts_with(SESSION)
    );
    assert_eq!(
        m["files"],
        serde_json::to_value(&out.manifest.files).unwrap()
    );
    assert!(verify_archive(&out.dest_dir).unwrap().ok);
}

#[test]
fn stops_on_first_failure_and_records_stderr() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let (bin, log) = fake_weaver(tools.path());
    let mut t = StoreTarget::new("lab", "fail-prefix", "weftos:w1");
    t.weaver_bin = Some(bin);
    let out = archive_session(&request(src.path(), dest.path(), t)).unwrap();

    let up = out.store_upload.unwrap();
    assert!(!up.ok);
    assert_eq!(argv(&log).len(), 1, "no call after the first failure");
    assert_eq!(up.objects[0].status, ObjectStatus::Failed);
    assert!(
        up.objects[0]
            .stderr_tail
            .as_deref()
            .unwrap()
            .contains("denied by grant")
    );
    assert!(
        up.objects[1..]
            .iter()
            .all(|o| o.status == ObjectStatus::Skipped)
    );
    assert_eq!(up.objects.last().unwrap().file, "manifest.json");
    assert!(verify_archive(&out.dest_dir).unwrap().ok);
    assert_eq!(read_manifest(&out.dest_dir)["store_upload"]["ok"], false);
}

#[test]
fn dry_run_lists_commands_and_runs_nothing() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    let (bin, log) = fake_weaver(tools.path());
    let mut t = StoreTarget::new("lab", "p", "weftos:w1");
    t.weaver_bin = Some(bin.clone());
    t.dry_run = true;
    let out = archive_session(&request(src.path(), dest.path(), t)).unwrap();

    let up = out.store_upload.unwrap();
    assert!(up.ok && up.dry_run);
    assert!(!log.exists(), "the binary was not run");
    assert_eq!(up.commands.len(), out.manifest.files.len() + 1);
    let key = format!("p/{SESSION}/{}", out.manifest.files[0].name);
    assert_eq!(up.commands[0][0], bin);
    assert_eq!(
        up.commands[0][1..7],
        ["store", "put", "lab", key.as_str(), "--as", "weftos:w1"]
    );
    assert!(up.objects.iter().all(|o| o.status == ObjectStatus::DryRun));
    assert!(read_manifest(&out.dest_dir).get("store_upload").is_none());
}

#[test]
fn missing_binary_is_a_recorded_failure_and_no_store_means_no_step() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let mut t = StoreTarget::new("lab", "p", "weftos:w1");
    t.weaver_bin = Some("/nonexistent/weaver".into());
    let out = archive_session(&request(src.path(), dest.path(), t)).unwrap();
    let up = out.store_upload.unwrap();
    assert!(!up.ok);
    assert!(
        up.objects[0]
            .stderr_tail
            .as_deref()
            .unwrap()
            .contains("cannot run")
    );

    let src2 = session_copy();
    let dest2 = tempfile::tempdir().unwrap();
    let mut r = ArchiveRequest::new(src2.path(), dest2.path());
    r.consent = Some(json!({"given": true}));
    assert!(archive_session(&r).unwrap().store_upload.is_none());
}
