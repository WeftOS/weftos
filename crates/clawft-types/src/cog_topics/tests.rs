use std::path::PathBuf;

use serde_json::{Value, json};

use super::*;

fn vectors_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/cogs/vectors")
}

fn vector_files(schema: &str, prefix: &str) -> Vec<PathBuf> {
    let dir = vectors_root().join(schema);
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with(prefix) && n.ends_with(".json")
        })
        .collect();
    out.sort();
    out
}

fn load(p: &PathBuf) -> Value {
    serde_json::from_slice(&std::fs::read(p).unwrap()).unwrap()
}

fn typed_schemas() -> impl Iterator<Item = &'static str> {
    SCHEMA_IDS.into_iter().filter(|s| *s != "spatial.evidence.v1")
}

#[test]
fn valid_vectors_round_trip_and_validate() {
    for schema in typed_schemas() {
        let files = vector_files(schema, "valid-");
        assert!(!files.is_empty(), "{schema}: no valid vectors");
        for f in files {
            let msg = load(&f);
            let back = check_message(schema, &msg)
                .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
            assert_eq!(back, msg, "{} does not round-trip", f.display());
        }
    }
}

#[test]
fn invalid_vectors_are_rejected() {
    for schema in typed_schemas() {
        let files = vector_files(schema, "invalid-");
        assert!(!files.is_empty(), "{schema}: no invalid vectors");
        for f in files {
            assert!(
                check_message(schema, &load(&f)).is_err(),
                "{} was accepted",
                f.display()
            );
        }
    }
}

#[test]
fn every_vector_directory_is_a_registered_schema() {
    for e in std::fs::read_dir(vectors_root()).unwrap() {
        let name = e.unwrap().file_name().to_string_lossy().into_owned();
        assert!(is_registered(&name), "vectors/{name} is not a registered id");
    }
}

#[test]
fn readme_names_every_schema() {
    let readme = std::fs::read_to_string(vectors_root().join("../README.md")).unwrap();
    for id in SCHEMA_IDS {
        assert!(readme.contains(id), "README.md does not mention {id}");
    }
}

#[test]
fn registry_ids_are_unique() {
    let mut ids = SCHEMA_IDS.to_vec();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), SCHEMA_IDS.len());
    assert!(is_registered("senml.v1") && !is_registered("senml.v2"));
}

#[test]
fn unregistered_and_external_schemas_are_not_checked_here() {
    let msg = json!({});
    assert!(check_message("nope.v1", &msg).is_err());
    assert!(check_message("spatial.evidence.v1", &msg).is_err());
}

#[test]
fn topic_kind_grammar() {
    for ok in ["audio.pcm", "a", "video.frame-ref", "can-fd.rx0", "a.0-1"] {
        assert!(valid_topic_kind(ok), "{ok}");
    }
    for bad in ["", "Audio", "1a", "-a", "a..b", "a.", ".a", "a_b", "a b"] {
        assert!(!valid_topic_kind(bad), "{bad}");
    }
    assert!(valid_topic_kind(&"a".repeat(64)) && !valid_topic_kind(&"a".repeat(65)));
}

#[test]
fn class_and_policy_parse() {
    assert_eq!(TopicClass::parse("stream"), Some(TopicClass::Stream));
    assert_eq!(TopicClass::parse("Stream"), None);
    assert_eq!(InputPolicy::parse("drop-oldest"), Some(InputPolicy::DropOldest));
    assert_eq!(InputPolicy::default(), InputPolicy::DropOldest);
    assert_eq!(InputPolicy::parse("drop"), None);
}

#[test]
fn store_vector_is_the_ingest_body_with_an_envelope() {
    let mut msg = load(&vector_files("store.vector.v1", "valid-one")[0]);
    let back = check_message("store.vector.v1", &msg).unwrap();
    assert_eq!(back["vectors"][0][1].as_array().unwrap().len(), 8);
    // One value past f32 is refused even though it is a finite f64.
    msg["vectors"][0][1][0] = json!(1.0e39);
    assert!(check_message("store.vector.v1", &msg).is_err());
}

#[test]
fn sdr_replies_validate_and_round_trip() {
    for f in vector_files("sdr.v1", "reply-valid-") {
        let v = load(&f);
        let r: SdrReply = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        r.validate().unwrap();
        assert_eq!(serde_json::to_value(&r).unwrap(), v, "{}", f.display());
    }
    let bad: SdrReply = serde_json::from_value(json!({"ok": false})).unwrap();
    assert!(bad.validate().is_err());
}

#[test]
fn iq_gap_record_is_not_a_chunk_but_is_recognised() {
    let gap = load(&vectors_root().join("iq.cs16.v1/gap-record.json"));
    assert!(is_gap_record(&gap));
    assert!(check_message("iq.cs16.v1", &gap).is_err());
    assert!(!is_gap_record(&load(&vectors_root().join("iq.cs16.v1/valid-single-sample.json"))));
}

#[test]
fn handles_are_found_nested_in_bodies() {
    let h = |c: char| json!({"hash": c.to_string().repeat(64), "size": 9, "media_type": "image/png", "expires_ms": 5});
    let body = json!({"text": "done", "images": [h('a'), {"deep": h('b')}], "bad": {"hash": "zz", "media_type": "image/png"}});
    let found = find_handles(&body);
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].hash, "a".repeat(64));
    assert!(find_handles(&json!({"hash": "a".repeat(64), "media_type": "IMAGE"})).is_empty());
}

#[test]
fn media_types() {
    for ok in ["image/png", "application/octet-stream", "audio/l16", "image/svg+xml"] {
        assert!(valid_media_type(ok), "{ok}");
    }
    for bad in ["", "image", "/png", "Image/png", "image/", "image/p ng"] {
        assert!(!valid_media_type(bad), "{bad}");
    }
}
