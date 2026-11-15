//! RuView on-disk names (verified against its sensing-server recording code,
//! 2026-11-14): `<session>.csi.jsonl`, the `<session>.csi.meta.json` sidecar
//! written at stop, and the session id charset.

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use weftos_worldmodel_dataset::{
    ArchiveRequest, DatasetError, SessionLayout, archive_session, verify_archive,
};

const SESSION: &str = "demo-session-001";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn session_copy() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    for e in fs::read_dir(fixtures().join("session")).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), td.path().join(e.file_name())).unwrap();
    }
    td
}

fn request(session: &Path, dest: &Path) -> ArchiveRequest {
    let mut r = ArchiveRequest::new(session, dest);
    r.consent = Some(json!({"given": true}));
    r
}

fn meta_path(dir: &Path) -> PathBuf {
    dir.join(format!("{SESSION}.csi.meta.json"))
}

fn edit_meta(dir: &Path, f: impl FnOnce(&mut Value)) {
    let p = meta_path(dir);
    let mut m: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    f(&mut m);
    fs::write(&p, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
}

#[test]
fn defaults_are_the_ruview_names() {
    let l = SessionLayout::default();
    assert_eq!(l.csi[0], "{session}.csi.jsonl");
    assert_eq!(l.csi[1], "{session}.csi*");
    assert_eq!(l.csi_meta, "{session}.csi.meta.json");
    assert_eq!(
        l.finished_marker.as_deref(),
        Some("{session}.csi.meta.json")
    );
    assert!(l.require_finished);
    assert_eq!(l.evidence, "{session}.evidence.jsonl");
}

#[test]
fn sidecar_is_parsed_checked_and_archived() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    let rec = out.manifest.csi_recording.as_ref().unwrap();
    assert_eq!(rec.id, SESSION);
    assert_eq!(rec.name, "demo");
    assert_eq!(rec.label, None);
    assert_eq!(rec.frame_count, 4);
    assert_eq!(rec.ended_at.as_deref(), Some("2025-10-03T14:00:04+00:00"));
    let csi = out.manifest.files.iter().find(|f| f.role == "csi").unwrap();
    assert_eq!(csi.name, format!("{SESSION}.csi.jsonl"));
    assert_eq!(csi.bytes, rec.file_size_bytes);
    assert!(out.manifest.files.iter().any(|f| f.role == "csi_meta"));
    assert!(out.manifest.finished);
    assert!(verify_archive(&out.dest_dir).unwrap().ok);
}

#[test]
fn marker_absent_is_refused_by_default_and_present_passes() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    assert!(archive_session(&request(src.path(), dest.path())).is_ok());
    let src2 = session_copy();
    fs::remove_file(meta_path(src2.path())).unwrap();
    assert!(matches!(
        archive_session(&request(src2.path(), dest.path())),
        Err(DatasetError::NotFinished(_))
    ));
}

#[test]
fn sidecar_size_mismatch_is_refused() {
    let src = session_copy();
    edit_meta(src.path(), |m| m["file_size_bytes"] = json!(1));
    let dest = tempfile::tempdir().unwrap();
    match archive_session(&request(src.path(), dest.path())) {
        Err(DatasetError::CsiMetaMismatch(why)) => {
            assert!(why.contains("file_size_bytes"), "{why}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!dest.path().join("ruview-demo").exists());
}

#[test]
fn sidecar_id_mismatch_is_refused() {
    let src = session_copy();
    edit_meta(src.path(), |m| m["id"] = json!("other-20261114_101500"));
    let dest = tempfile::tempdir().unwrap();
    match archive_session(&request(src.path(), dest.path())) {
        Err(DatasetError::CsiMetaMismatch(why)) => assert!(why.contains("sidecar id"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn sidecar_without_its_csi_file_is_refused() {
    let src = session_copy();
    fs::remove_file(src.path().join(format!("{SESSION}.csi.jsonl"))).unwrap();
    let dest = tempfile::tempdir().unwrap();
    assert!(matches!(
        archive_session(&request(src.path(), dest.path())),
        Err(DatasetError::CsiMetaMismatch(_))
    ));
}

#[test]
fn unparseable_sidecar_is_refused() {
    let src = session_copy();
    fs::write(meta_path(src.path()), "{not json").unwrap();
    let dest = tempfile::tempdir().unwrap();
    assert!(matches!(
        archive_session(&request(src.path(), dest.path())),
        Err(DatasetError::Json { .. })
    ));
}

fn protocol_of(src: &Path) -> Option<Value> {
    let dest = tempfile::tempdir().unwrap();
    archive_session(&request(src, dest.path()))
        .unwrap()
        .manifest
        .protocol
}

#[test]
fn protocol_falls_back_label_then_keyword_then_null() {
    let src = session_copy();
    assert_eq!(protocol_of(src.path()), None, "no label, no keyword");

    edit_meta(src.path(), |m| m["label"] = json!("seated"));
    assert_eq!(protocol_of(src.path()), Some(json!("seated")));

    // The explicit value and a protocol file both outrank the label.
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.protocol = Some("operator text".into());
    let m = archive_session(&req).unwrap().manifest;
    assert_eq!(m.protocol, Some(json!("operator text")));
    assert_eq!(m.csi_recording.unwrap().label.as_deref(), Some("seated"));

    // A blank label falls through to the keyword in the session name.
    let kw = tempfile::tempdir().unwrap();
    for e in fs::read_dir(src.path()).unwrap() {
        let e = e.unwrap();
        let name = e
            .file_name()
            .to_string_lossy()
            .replace(SESSION, "walking-20261114_101500");
        fs::copy(e.path(), kw.path().join(name)).unwrap();
    }
    let p = kw.path().join("walking-20261114_101500.csi.meta.json");
    let mut m: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    m["id"] = json!("walking-20261114_101500");
    m["label"] = json!(" ");
    m["file_path"] = json!("data/recordings/walking-20261114_101500.csi.jsonl");
    fs::write(&p, serde_json::to_vec(&m).unwrap()).unwrap();
    assert_eq!(protocol_of(kw.path()), Some(json!("walking")));
}

#[test]
fn dotted_session_id_is_accepted_and_inferred() {
    let src = tempfile::tempdir().unwrap();
    let id = "walking.v2-20261114_101500";
    for e in fs::read_dir(fixtures().join("session")).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().replace(SESSION, id);
        fs::copy(e.path(), src.path().join(name)).unwrap();
    }
    let p = src.path().join(format!("{id}.csi.meta.json"));
    let mut m: Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    m["id"] = json!(id);
    fs::write(&p, serde_json::to_vec(&m).unwrap()).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    assert_eq!(out.manifest.session, id);
    assert_eq!(
        out.manifest.protocol,
        Some(json!("walking")),
        "keyword token in the id"
    );
    assert!(out.dest_dir.ends_with(id));
}

#[test]
fn bad_session_ids_still_refused() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    for bad in ["../evil", "..", ".hidden", "a/b", "a\\b", "a b", ""] {
        let mut req = request(src.path(), dest.path());
        req.session = Some(bad.into());
        assert!(
            matches!(archive_session(&req), Err(DatasetError::InvalidSession(_))),
            "{bad:?}"
        );
    }
}

fn two_regions(src: &Path) {
    let p = src.join(format!("{SESSION}.evidence.jsonl"));
    let text = fs::read_to_string(&p)
        .unwrap()
        .replacen("meso/test-room", "meso/other-room", 1);
    fs::write(&p, text).unwrap();
}

#[test]
fn multi_region_is_opt_in_and_records_every_region() {
    let src = session_copy();
    two_regions(src.path());
    let dest = tempfile::tempdir().unwrap();
    assert!(matches!(
        archive_session(&request(src.path(), dest.path())),
        Err(DatasetError::MultipleRegions(_))
    ));
    let mut req = request(src.path(), dest.path());
    req.allow_multi_region = true;
    let out = archive_session(&req).unwrap();
    assert_eq!(
        out.manifest.regions,
        ["region/urth/meso/other-room", "region/urth/meso/test-room"]
    );
    assert_eq!(out.manifest.room, "region/urth/meso/other-room");
    assert!(out.verification.ok);
    assert!(verify_archive(&out.dest_dir).unwrap().ok);

    // Dropping a region from the manifest is caught by verify.
    let mp = out.dest_dir.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&fs::read(&mp).unwrap()).unwrap();
    m["regions"] = json!(["region/urth/meso/other-room"]);
    fs::write(&mp, serde_json::to_vec(&m).unwrap()).unwrap();
    let v = verify_archive(&out.dest_dir).unwrap();
    assert!(!v.ok && v.manifest_problems.iter().any(|p| p.contains("regions")));
}

#[test]
fn edited_csi_recording_is_caught_by_verify() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    let mp = out.dest_dir.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&fs::read(&mp).unwrap()).unwrap();
    m["csi_recording"]["frame_count"] = json!(9999);
    fs::write(&mp, serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(!verify_archive(&out.dest_dir).unwrap().ok);
}

#[test]
fn legacy_csi_bin_is_found_by_the_glob_fallback() {
    let src = session_copy();
    fs::remove_file(src.path().join(format!("{SESSION}.csi.jsonl"))).unwrap();
    fs::remove_file(meta_path(src.path())).unwrap();
    fs::copy(
        fixtures().join(format!("legacy/{SESSION}.csi.bin")),
        src.path().join(format!("{SESSION}.csi.bin")),
    )
    .unwrap();
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.layout.require_finished = false;
    let out = archive_session(&req).unwrap();
    let names: Vec<_> = out
        .manifest
        .files
        .iter()
        .filter(|f| f.role == "csi")
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(names, [format!("{SESSION}.csi.bin")]);
    assert!(out.manifest.csi_recording.is_none());
}

/// An archive made before this change: previous layout, no sidecar, no
/// `regions` or `csi_recording`, no `csi_meta` layout field.
#[test]
fn archive_made_with_the_previous_layout_still_verifies() {
    let src = session_copy();
    fs::remove_file(src.path().join(format!("{SESSION}.csi.jsonl"))).unwrap();
    fs::remove_file(meta_path(src.path())).unwrap();
    fs::copy(
        fixtures().join(format!("legacy/{SESSION}.csi.bin")),
        src.path().join(format!("{SESSION}.csi.bin")),
    )
    .unwrap();
    fs::write(src.path().join(format!("{SESSION}.done")), "").unwrap();
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.layout.csi = vec!["{session}.csi*".into(), "{session}*.csi.*".into()];
    req.layout.finished_marker = Some("{session}.done".into());
    req.layout.require_finished = false;
    let out = archive_session(&req).unwrap();

    let mp = out.dest_dir.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&fs::read(&mp).unwrap()).unwrap();
    for k in ["regions", "csi_recording"] {
        m.as_object_mut().unwrap().remove(k);
    }
    m["layout"].as_object_mut().unwrap().remove("csi_meta");
    m["layout"]
        .as_object_mut()
        .unwrap()
        .remove("require_finished");
    fs::write(&mp, serde_json::to_vec(&m).unwrap()).unwrap();

    assert!(verify_archive(&out.dest_dir).unwrap().ok);
    let loaded: weftos_worldmodel_dataset::Manifest =
        serde_json::from_slice(&fs::read(&mp).unwrap()).unwrap();
    assert!(loaded.regions.is_empty() && loaded.csi_recording.is_none());
    assert!(
        !loaded.layout.require_finished,
        "old manifests load as not required"
    );
}
