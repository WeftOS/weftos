//! Every key S4 writes must be a legal, case-stable file name on a RustFS that runs on a Windows host
//! (weftos-drive's finding, 2026-11-08): no `: * ? " | < >` or backslash or control character, no path
//! component ending in `.` or space, no Windows device name (`con`, `nul`, `com1`...), and no two keys that
//! differ only by case. Hostile build ids are refused before anything is written.

use weftos_pq_sign::MlDsa65SigningKey;
use weftos_release::*;

const NOW: u64 = 1_000;

fn signer() -> ReleaseSigner {
    ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32]), MlDsa65SigningKey::from_seed(&[7; 32]))
}

fn store() -> ReleaseStoreName {
    ReleaseStoreName::new("pg-mesh", "01JBXZ3N5C8QW4M2R7T9V6YH0K", "stable").unwrap()
}

fn req(build: &str, at: u64, files: &[(&str, &[u8])]) -> PublishRequest {
    PublishRequest {
        build_id: build.into(),
        source_rev: "abc".into(),
        created_at: at,
        builder: "ci".into(),
        artifacts: files.iter().map(|(p, b)| Artifact { path: (*p).into(), bytes: b.to_vec() }).collect(),
    }
}

const DEVICES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6",
    "lpt7", "lpt8", "lpt9",
];

/// Why `key` would not survive as a file on a case-folding Windows host, if it would not.
fn windows_problem(key: &str) -> Option<String> {
    if key.is_empty() || key.len() > 400 {
        return Some("length".into());
    }
    if let Some(c) = key.chars().find(|c| ":*?\"|<>\\".contains(*c) || c.is_control()) {
        return Some(format!("forbidden character {c:?}"));
    }
    for part in key.split('/') {
        if part.is_empty() || part.ends_with('.') || part.ends_with(' ') {
            return Some(format!("component {part:?} is empty or ends in '.' or ' '"));
        }
        let stem = part.split('.').next().unwrap_or("").to_ascii_lowercase();
        if DEVICES.contains(&stem.as_str()) {
            return Some(format!("component {part:?} is a Windows device name"));
        }
    }
    None
}

fn assert_portable(keys: &std::collections::BTreeSet<String>) {
    for k in keys {
        assert_eq!(windows_problem(k), None, "key {k}");
    }
    let mut folded = std::collections::BTreeMap::new();
    for k in keys {
        if let Some(other) = folded.insert(k.to_ascii_lowercase(), k) {
            panic!("keys {other} and {k} differ only by case");
        }
    }
}

#[test]
fn the_checker_itself_catches_what_windows_rejects() {
    for bad in ["a:b", "a*b", "a?b", "a\"b", "a|b", "a<b", "a>b", "a\\b", "x/y.", "releases/con/manifest.json", "nul.txt", "a/ /b", ""] {
        assert!(windows_problem(bad).is_some(), "{bad:?} should be flagged");
    }
    assert!(windows_problem("releases/b1/manifest.json").is_none());
}

#[tokio::test]
async fn every_key_a_publish_tombstone_and_sweep_writes_is_portable() {
    let (be, st, sg) = (MemoryBackend::new(), store(), signer());
    let tk = TrustedKeys(vec![sg.trusted()]);
    let p = ReleasePublisher::new(&be, &st, &sg);
    for (i, b) in ["b1", "build-2", "v1.2.3_rc.1", "2026-11-08.7"].iter().enumerate() {
        p.publish(req(b, 100 + i as u64, &[("bin/weaver", b"one"), ("lib/x.so", format!("p{i}").as_bytes())]), NOW).await.unwrap();
    }
    let mut seen: std::collections::BTreeSet<String> = be.keys().into_iter().collect();
    // keep 1, grace 0: tombstones first, then the blobs go through quarantine
    let keep = RetentionPolicy::new(1, 0).unwrap();
    for _ in 0..3 {
        Sweeper::new(&be, &st, &tk).sweep(&keep, 1_000, 500, "t").await.unwrap();
        seen.extend(be.keys());
    }
    assert!(seen.iter().any(|k| k.starts_with("tombstones/")) && seen.iter().any(|k| k.starts_with("releases/")));
    assert_portable(&seen);
}

#[tokio::test]
async fn hostile_build_ids_are_refused_before_anything_is_written() {
    let (be, st, sg) = (MemoryBackend::new(), store(), signer());
    let p = ReleasePublisher::new(&be, &st, &sg);
    let hostile = [
        "a:b", "a*b", "a?b", "a\"b", "a|b", "a<b", "a>b", "a\\b", "a/b",
        "con", "CON", "nul", "aux", "prn", "com1", "lpt9", "con.txt", "nul.json",
        "build.", "build1.", // trailing dot is dropped by Windows
        "Build1", "BUILD1", "bUILD", // case: would fold onto "build1" there
        ".hidden", "-x", "a..b", "",
    ];
    for id in hostile {
        let e = p.publish(req(id, 100, &[("a", b"x")]), NOW).await.unwrap_err();
        assert!(matches!(e, ReleaseError::Invalid { .. }), "{id:?}: {e}");
    }
    assert!(be.keys().is_empty(), "nothing written for any of them");
    // the lookalikes that are fine stay fine
    for id in ["console", "connect", "nulled", "com10", "com", "lpt", "a.b", "a-b_c", "0"] {
        p.publish(req(id, 100, &[("a", id.as_bytes())]), NOW).await.unwrap();
    }
    assert_portable(&be.keys().into_iter().collect());
}
