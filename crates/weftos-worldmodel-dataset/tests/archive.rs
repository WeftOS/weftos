use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use weftos_worldmodel_dataset::{
    ArchiveRequest, DatasetError, Manifest, archive_session, verify_archive,
};

const SESSION: &str = "demo-session-001";

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session")
}

/// Copy the fixture session into a fresh temp dir the test may modify.
fn session_copy() -> tempfile::TempDir {
    let td = tempfile::tempdir().unwrap();
    for e in fs::read_dir(fixture_dir()).unwrap() {
        let e = e.unwrap();
        fs::copy(e.path(), td.path().join(e.file_name())).unwrap();
    }
    td
}

fn request(session: &Path, dest: &Path) -> ArchiveRequest {
    let mut r = ArchiveRequest::new(session, dest);
    r.consent = Some(json!({"subject":"operator","scope":"world-model training","given":true}));
    r
}

#[test]
fn happy_path_manifest_counts_and_checksums() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.protocol = Some("empty, walking, seated phases".into());
    let out = archive_session(&req).unwrap();

    assert!(out.verification.ok);
    assert_eq!(
        out.dest_dir,
        dest.path().join("ruview-demo/datasets").join(SESSION)
    );
    let m = &out.manifest;
    assert_eq!(m.manifest_version, "lewm.dataset.v1");
    assert_eq!(m.session, SESSION);
    assert_eq!(m.room, "region/urth/meso/test-room");
    assert_eq!(m.evidence.lines, 20);
    assert_eq!(m.evidence.by_type["pose"].lines, 2);
    assert_eq!(m.evidence.by_type["radar_track_point"].lines, 5);
    assert_eq!(m.evidence.by_type["tof_depth"].lines, 4);
    assert_eq!(m.evidence.by_type["human_confirm"].lines, 3);
    assert_eq!(m.evidence.by_type["rf_gaussian"].proof.code, 5);
    assert_eq!(m.evidence.proof_total.measured, 15);
    assert_eq!(m.evidence.proof_total.code, 5);
    assert_eq!(m.evidence.proof_total.synthetic, 0);
    assert!(m.evidence.unknown_types.is_empty());
    assert_eq!(m.shell_measure.len(), 1);
    assert_eq!(m.shell_measure[0]["max"], json!([6.4, 3.66, 2.44]));
    assert_eq!(
        m.node_poses.keys().cloned().collect::<Vec<_>>(),
        ["node-1", "node-2"]
    );
    assert_eq!(m.protocol, Some(json!("empty, walking, seated phases")));
    assert_eq!(m.consent["given"], true);
    assert_eq!(m.time_span.evidence.min_t_ns, 1_759_500_000_000_000_000);
    assert_eq!(m.time_span.evidence.max_t_ns, 1_759_500_004_400_000_000);
    let r = m.readings.as_ref().unwrap();
    assert_eq!((r.packs, r.records), (4, 6));
    assert!(m.missing.is_empty());
    assert!(chrono::DateTime::parse_from_rfc3339(&m.created_at).is_ok());
    assert_eq!(m.archiver.name, "weftos-worldmodel-dataset");

    // Every archived file matches its source byte for byte.
    for f in &m.files {
        assert_eq!(
            fs::read(src.path().join(&f.name)).unwrap(),
            fs::read(out.dest_dir.join(&f.name)).unwrap()
        );
    }
    assert_eq!(m.files.len(), 3);
    let on_disk: Manifest =
        serde_json::from_slice(&fs::read(out.dest_dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(&on_disk, m);
    assert!(verify_archive(&out.dest_dir).unwrap().ok);
    // No temp directory is left behind.
    let leftovers: Vec<_> = fs::read_dir(out.dest_dir.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn consent_file_and_protocol_file_are_used_and_copied() {
    let src = session_copy();
    fs::write(
        src.path().join("consent.json"),
        r#"{"given":true,"by":"owner"}"#,
    )
    .unwrap();
    fs::write(src.path().join("protocol.txt"), "walk the room").unwrap();
    let dest = tempfile::tempdir().unwrap();
    let mut req = ArchiveRequest::new(src.path(), dest.path());
    req.protocol = Some("ignored".into());
    let out = archive_session(&req).unwrap();
    assert_eq!(out.manifest.consent["by"], "owner");
    assert_eq!(out.manifest.protocol, Some(json!("walk the room")));
    assert!(out.dest_dir.join("consent.json").exists());
}

#[test]
fn missing_optional_files_are_recorded() {
    let src = session_copy();
    fs::remove_file(src.path().join(format!("{SESSION}.csi.bin"))).unwrap();
    fs::remove_file(src.path().join(format!("{SESSION}.readings.jsonl"))).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    assert_eq!(out.manifest.missing, ["csi", "readings"]);
    assert!(out.manifest.readings.is_none());
}

#[test]
fn bad_evidence_line_aborts_with_line_number_and_publishes_nothing() {
    let src = session_copy();
    let p = src.path().join(format!("{SESSION}.evidence.jsonl"));
    let mut lines: Vec<String> = fs::read_to_string(&p)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    lines[6] = lines[6].replace("room_enu", "world");
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    let dest = tempfile::tempdir().unwrap();
    match archive_session(&request(src.path(), dest.path())) {
        Err(DatasetError::Evidence { line, .. }) => assert_eq!(line, 7),
        other => panic!("{other:?}"),
    }
    assert!(!dest.path().join("ruview-demo").exists());
}

#[test]
fn missing_consent_refuses() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let req = ArchiveRequest::new(src.path(), dest.path());
    assert!(matches!(
        archive_session(&req),
        Err(DatasetError::ConsentMissing)
    ));
    let mut blank = ArchiveRequest::new(src.path(), dest.path());
    blank.consent = Some(json!("  "));
    assert!(matches!(
        archive_session(&blank),
        Err(DatasetError::ConsentMissing)
    ));
    assert!(!dest.path().join("ruview-demo").exists());
}

#[test]
fn two_regions_refuse() {
    let src = session_copy();
    let p = src.path().join(format!("{SESSION}.evidence.jsonl"));
    let text = fs::read_to_string(&p)
        .unwrap()
        .replacen("meso/test-room", "meso/other-room", 1);
    fs::write(&p, text).unwrap();
    let dest = tempfile::tempdir().unwrap();
    match archive_session(&request(src.path(), dest.path())) {
        Err(DatasetError::MultipleRegions(r)) => assert_eq!(r.len(), 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn unknown_type_is_counted_not_fatal() {
    let src = session_copy();
    let p = src.path().join(format!("{SESSION}.evidence.jsonl"));
    let mut text = fs::read_to_string(&p).unwrap();
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let mut extra = first.clone();
    extra["type"] = json!("thermal_blob");
    extra["provenance"]["receipt"] = json!("thermal-1");
    text.push_str(&format!("{extra}\n"));
    fs::write(&p, text).unwrap();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    assert_eq!(out.manifest.evidence.unknown_types["thermal_blob"], 1);
    assert_eq!(out.manifest.evidence.lines, 21);
}

#[test]
fn existing_destination_refused_without_overwrite() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let first = archive_session(&request(src.path(), dest.path())).unwrap();
    fs::write(first.dest_dir.join("marker"), "x").unwrap();
    assert!(matches!(
        archive_session(&request(src.path(), dest.path())),
        Err(DatasetError::DestinationExists(_))
    ));
    assert!(
        first.dest_dir.join("marker").exists(),
        "existing dataset untouched"
    );
    let mut over = request(src.path(), dest.path());
    over.overwrite = true;
    let again = archive_session(&over).unwrap();
    assert!(again.verification.ok);
    assert!(!again.dest_dir.join("marker").exists());
}

#[test]
fn corrupted_copy_is_detected() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    let csi = out.dest_dir.join(format!("{SESSION}.csi.bin"));
    let bytes = fs::read(&csi).unwrap();
    fs::write(&csi, &bytes[..bytes.len() / 2]).unwrap();
    let v = verify_archive(&out.dest_dir).unwrap();
    assert!(!v.ok);
    let bad: Vec<_> = v
        .files
        .iter()
        .filter(|f| !f.ok)
        .map(|f| f.name.as_str())
        .collect();
    assert_eq!(bad, [format!("{SESSION}.csi.bin")]);
    fs::remove_file(&csi).unwrap();
    assert!(!verify_archive(&out.dest_dir).unwrap().ok);
}

#[test]
fn session_inferred_and_custom_layout() {
    let src = tempfile::tempdir().unwrap();
    for (from, to) in [
        ("evidence.jsonl", "run9.ev.jsonl"),
        ("csi.bin", "run9.capture.dat"),
    ] {
        fs::copy(
            fixture_dir().join(format!("{SESSION}.{from}")),
            src.path().join(to),
        )
        .unwrap();
    }
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.layout.evidence = "{session}.ev.jsonl".into();
    req.layout.csi = vec!["{session}.capture.*".into()];
    let out = archive_session(&req).unwrap();
    assert_eq!(out.manifest.session, "run9");
    assert_eq!(out.manifest.layout.evidence, "{session}.ev.jsonl");
    assert_eq!(out.manifest.missing, ["readings"]);
}

#[test]
fn traversal_session_id_refused() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.session = Some("../evil".into());
    assert!(matches!(
        archive_session(&req),
        Err(DatasetError::InvalidSession(_))
    ));
}

fn evidence_path(dir: &Path) -> PathBuf {
    dir.join(format!("{SESSION}.evidence.jsonl"))
}

#[test]
fn duplicate_receipt_across_lines_aborts_with_both_line_numbers() {
    let src = session_copy();
    let p = evidence_path(src.path());
    let mut lines: Vec<String> = fs::read_to_string(&p)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let v: Value = serde_json::from_str(&lines[2]).unwrap();
    let receipt = v["provenance"]["receipt"].as_str().unwrap().to_string();
    let mut dup: Value = serde_json::from_str(&lines[9]).unwrap();
    dup["provenance"]["receipt"] = json!(receipt);
    lines[9] = dup.to_string();
    fs::write(&p, lines.join("\n") + "\n").unwrap();
    let dest = tempfile::tempdir().unwrap();
    match archive_session(&request(src.path(), dest.path())) {
        Err(DatasetError::Evidence { line, reason, .. }) => {
            assert_eq!(line, 10);
            assert!(
                reason.contains("line 3") && reason.contains("line 10"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(!dest.path().join("ruview-demo").exists());
}

#[test]
fn finished_marker_is_recorded_and_not_archived_as_csi() {
    let src = session_copy();
    fs::write(src.path().join(format!("{SESSION}.done")), "").unwrap();
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.layout.require_finished = true;
    let out = archive_session(&req).unwrap();
    assert!(out.manifest.finished);
    assert!(out.manifest.layout.require_finished);
    assert_eq!(out.manifest.files.len(), 3);
}

#[test]
fn absent_marker_refused_only_when_required() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    let out = archive_session(&req).unwrap();
    assert!(
        !out.manifest.finished,
        "default does not require the marker"
    );

    req.overwrite = true;
    req.layout.require_finished = true;
    match archive_session(&req) {
        Err(DatasetError::NotFinished(p)) => {
            assert!(p.ends_with(format!("{SESSION}.done")), "{p:?}");
        }
        other => panic!("{other:?}"),
    }
    // Existing dataset untouched by the refused run.
    assert!(verify_archive(&out.dest_dir).unwrap().ok);

    // A disabled marker cannot satisfy require_finished.
    req.layout.finished_marker = None;
    assert!(matches!(
        archive_session(&req),
        Err(DatasetError::BadLayout(..))
    ));
    req.layout.require_finished = false;
    assert!(archive_session(&req).is_ok());
}

#[test]
fn custom_marker_template() {
    let src = session_copy();
    fs::write(src.path().join("CLOSED"), "").unwrap();
    let dest = tempfile::tempdir().unwrap();
    let mut req = request(src.path(), dest.path());
    req.layout.finished_marker = Some("CLOSED".into());
    req.layout.require_finished = true;
    assert!(archive_session(&req).unwrap().manifest.finished);
}

#[test]
fn old_manifest_without_marker_fields_still_loads() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    let mp = out.dest_dir.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&fs::read(&mp).unwrap()).unwrap();
    m.as_object_mut().unwrap().remove("finished");
    let layout = m["layout"].as_object_mut().unwrap();
    layout.remove("finished_marker");
    layout.remove("require_finished");
    fs::write(&mp, serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(verify_archive(&out.dest_dir).unwrap().ok);
}

fn edit_manifest(dir: &Path, f: impl FnOnce(&mut Value)) {
    let mp = dir.join("manifest.json");
    let mut m: Value = serde_json::from_slice(&fs::read(&mp).unwrap()).unwrap();
    f(&mut m);
    fs::write(&mp, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
}

#[test]
fn edited_manifest_with_matching_bytes_is_caught() {
    let src = session_copy();
    let dest = tempfile::tempdir().unwrap();
    let out = archive_session(&request(src.path(), dest.path())).unwrap();
    assert!(out.verification.manifest_problems.is_empty());

    type Edit = Box<dyn Fn(&mut Value)>;
    let edits: [(&str, Edit); 5] = [
        ("lines", Box::new(|m| m["evidence"]["lines"] = json!(21))),
        (
            "proof",
            Box::new(|m| m["evidence"]["proof_total"]["MEASURED"] = json!(20)),
        ),
        (
            "by_type",
            Box::new(|m| m["evidence"]["by_type"]["pose"]["lines"] = json!(9)),
        ),
        (
            "readings",
            Box::new(|m| m["readings"]["records"] = json!(7)),
        ),
        (
            "room",
            Box::new(|m| m["room"] = json!("region/urth/meso/other")),
        ),
    ];
    for (name, edit) in edits {
        let pristine = fs::read(out.dest_dir.join("manifest.json")).unwrap();
        edit_manifest(&out.dest_dir, edit);
        let v = verify_archive(&out.dest_dir).unwrap();
        assert!(v.files.iter().all(|f| f.ok), "{name}: bytes still match");
        assert!(!v.ok, "{name}: edit must fail verify");
        assert!(!v.manifest_problems.is_empty(), "{name}");
        fs::write(out.dest_dir.join("manifest.json"), pristine).unwrap();
    }
    assert!(verify_archive(&out.dest_dir).unwrap().ok);
}
