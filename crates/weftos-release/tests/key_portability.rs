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

/// Length guard: derived from what the validators ACCEPT, not from constants copied into the test. It finds the
/// longest build id and the longest artifact-path segment the publisher takes, then checks (1) no path segment
/// of any key or artifact path is longer than a file name can be (255 bytes: Linux, macOS, Windows), (2) the
/// longest key plus its bucket leaves room under a Windows 260-character path for the data directory and
/// RustFS's own per-object files, and (3) it fits S1's 512-byte key rule with S1's archive prefix (78 bytes).
#[tokio::test]
async fn the_longest_key_s4_writes_stays_inside_the_segment_path_and_key_budgets() {
    const FILE_NAME_MAX: usize = 255;
    let (st, sg) = (store(), signer());
    let accepts_id = |n: usize| {
        let (st, sg) = (store(), signer());
        let id = "a".repeat(n);
        async move { ReleasePublisher::new(&MemoryBackend::new(), &st, &sg).publish(req(&id, 100, &[("x", b"x")]), NOW).await.is_ok() }
    };
    let mut id_len = 0;
    for n in (1..=400).rev() {
        if accepts_id(n).await {
            id_len = n;
            break;
        }
    }
    assert!(id_len > 0);

    // every key a publish, tombstone and sweep writes for the longest id
    let (be, tk) = (MemoryBackend::new(), TrustedKeys(vec![sg.trusted()]));
    ReleasePublisher::new(&be, &st, &sg).publish(req(&"a".repeat(id_len), 100, &[("x", b"x")]), NOW).await.unwrap();
    ReleasePublisher::new(&be, &st, &sg).publish(req("b2", 200, &[("y", b"y")]), NOW).await.unwrap();
    let keep = RetentionPolicy::new(1, 0).unwrap();
    let mut seen: std::collections::BTreeSet<String> = be.keys().into_iter().collect();
    for _ in 0..3 {
        Sweeper::new(&be, &st, &tk).sweep(&keep, 1_000, 500, "t").await.unwrap();
        seen.extend(be.keys());
    }
    // (1) segments of keys
    let longest_key_segment = seen.iter().flat_map(|k| k.split('/')).map(str::len).max().unwrap();
    assert!(longest_key_segment <= FILE_NAME_MAX, "a key segment is {longest_key_segment} bytes (a build id of {id_len} is accepted): over a file name");
    // (1) segments of artifact paths: the longest single-segment path the publisher takes
    let mut longest_artifact_segment = 0;
    for n in (1..=600).rev() {
        let be = MemoryBackend::new();
        let path = "s".repeat(n);
        if ReleasePublisher::new(&be, &st, &sg).publish(req("b1", 100, &[(&path, b"x")]), NOW).await.is_ok() {
            longest_artifact_segment = n;
            break;
        }
    }
    assert!(longest_artifact_segment > 0 && longest_artifact_segment <= FILE_NAME_MAX, "artifact path segments up to {longest_artifact_segment} bytes are accepted: over a file name");
    // (2) path budget: bucket of a release store is `r-` + 26-char project id + `-` + 12-hex channel hash
    let bucket = 2 + 26 + 1 + 12;
    let longest = seen.iter().map(String::len).max().unwrap();
    assert!(bucket + 1 + longest <= 200, "bucket+key is {} bytes (build id {id_len}); 200 leaves 60 of 260 for the data dir and RustFS's own files", bucket + 1 + longest);
    // (3) S1's archive prefix
    assert!(longest + 78 <= 512, "plus S1's archive prefix");
    // the pin: change the cap on purpose, re-check the three budgets, then move it
    assert_eq!(id_len, 128, "the build id cap moved");
}

/// Segment length (the 11:05 pile): no path segment of anything S4 writes may exceed the 255 bytes a file
/// name can have on Linux, macOS and Windows. Object keys are covered by the validators' own caps (build id
/// 128, hash 64, fixed prefixes); artifact paths, which become file names when installed, are capped per
/// segment in `validate_artifact_path`.
#[tokio::test]
async fn no_segment_of_a_key_or_artifact_path_exceeds_a_file_name() {
    // keys: the longest build id gives the longest segment
    let (be, st, sg) = (MemoryBackend::new(), store(), signer());
    ReleasePublisher::new(&be, &st, &sg).publish(req(&"a".repeat(128), 100, &[("x", b"x")]), NOW).await.unwrap();
    for k in be.keys() {
        assert!(k.split('/').all(|s| s.len() <= 255), "{k}");
    }
    assert!(be.keys().iter().flat_map(|k| k.split('/')).map(str::len).max().unwrap() <= 128);
    // artifact paths: 255 is fine, 256 is refused, in any position
    let seg = |n: usize| "s".repeat(n);
    let ok = format!("{}/{}", seg(255), seg(255));
    let p = ReleasePublisher::new(&be, &st, &sg);
    p.publish(req("b-ok", 100, &[(&ok, b"x")]), NOW).await.unwrap();
    for bad in [seg(256), format!("d/{}", seg(256)), format!("{}/f", seg(300))] {
        let e = p.publish(req("b-bad", 100, &[(&bad, b"x")]), NOW).await.unwrap_err();
        assert!(matches!(e, ReleaseError::Invalid { .. }), "{}: {e}", bad.len());
    }
    assert!(!be.keys().iter().any(|k| k.contains("b-bad")), "nothing written for the refused paths");
}
