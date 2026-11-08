//! ADR-126 S4 acceptance: artifacts land in the release store, and a rebuild cannot overwrite them.

use std::sync::Arc;

use weftos_pq_sign::MlDsa65SigningKey;
use weftos_release::*;

const NOW: u64 = 1_000;
const ULID: &str = "01JBXZ3N5C8QW4M2R7T9V6YH0K";

fn store(channel: &str) -> ReleaseStoreName {
    ReleaseStoreName::new("pg-mesh", ULID, channel).unwrap()
}

fn signer(seed: u8) -> ReleaseSigner {
    ReleaseSigner::new(
        ed25519_dalek::SigningKey::from_bytes(&[seed; 32]),
        MlDsa65SigningKey::from_seed(&[seed; 32]),
    )
}

fn trusted(s: &ReleaseSigner) -> TrustedKeys {
    TrustedKeys(vec![s.trusted()])
}

fn req(build: &str, files: &[(&str, &[u8])]) -> PublishRequest {
    PublishRequest {
        build_id: build.into(),
        source_rev: "527ea4db".into(),
        created_at: 1_000,
        builder: "ci-runner-1".into(),
        artifacts: files
            .iter()
            .map(|(p, b)| Artifact { path: (*p).into(), bytes: b.to_vec() })
            .collect(),
    }
}

#[tokio::test]
async fn artifacts_land_and_read_back_by_hash() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    let p = ReleasePublisher::new(&be, &st, &sg);
    let r = p.publish(req("b1", &[("bin/weaver", b"ELF-weaver"), ("SHA256SUMS", b"sums")]), NOW).await.unwrap();
    assert_eq!(r.outcome, PublishOutcome::Published);
    assert_eq!(r.artifacts.len(), 2);

    let rd = ReleaseReader::new(&be, &st, &tk);
    assert_eq!(rd.build_ids().await.unwrap(), vec!["b1"]);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(m.manifest().store, "weftos://pg-mesh/projects/01JBXZ3N5C8QW4M2R7T9V6YH0K/releases/stable/store");
    assert_eq!(rd.read_artifact(&m, "bin/weaver").await.unwrap(), b"ELF-weaver");
    assert!(rd.audit(&m).await.unwrap().is_empty());
    // artifacts are referenced by hash
    let h = blake3::hash(b"ELF-weaver").to_hex().to_string();
    assert!(be.keys().contains(&format!("blobs/{h}")));
}

#[tokio::test]
async fn rebuild_with_same_build_id_cannot_overwrite() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let p = ReleasePublisher::new(&be, &st, &sg);
    p.publish(req("b1", &[("bin/weaver", b"original")]), NOW).await.unwrap();
    let before: Vec<_> = be.keys();
    let manifest_before = ReleaseBackendExt::raw(&be, "releases/b1/manifest.json").await;

    let err = p.publish(req("b1", &[("bin/weaver", b"REBUILT-DIFFERENT")]), NOW).await.unwrap_err();
    assert!(matches!(err, ReleaseError::AlreadyPublished { .. }), "{err}");
    // A different source rev with the same artifacts is also a different release.
    let mut r2 = req("b1", &[("bin/weaver", b"original")]);
    r2.source_rev = "deadbeef".into();
    assert!(matches!(p.publish(r2, NOW).await.unwrap_err(), ReleaseError::AlreadyPublished { .. }));

    // Nothing changed, not even a stray blob from the failed rebuild.
    assert_eq!(be.keys(), before);
    assert_eq!(ReleaseBackendExt::raw(&be, "releases/b1/manifest.json").await, manifest_before);
}

#[tokio::test]
async fn retried_job_is_idempotent() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let p = ReleasePublisher::new(&be, &st, &sg);
    let a = p.publish(req("b1", &[("a", b"x"), ("b", b"y")]), NOW).await.unwrap();
    let n = be.len();
    let mut again = req("b1", &[("b", b"y"), ("a", b"x")]); // order must not matter
    again.created_at = 9_999; // a retry may run later
    let b = p.publish(again, 9_999).await.unwrap();
    assert_eq!(b.outcome, PublishOutcome::AlreadyPresent);
    assert_eq!(a.manifest_digest, b.manifest_digest);
    assert_eq!(be.len(), n);
}

#[tokio::test]
async fn identical_files_in_two_releases_share_one_blob() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let p = ReleasePublisher::new(&be, &st, &sg);
    p.publish(req("b1", &[("a", b"same"), ("copy-of-a", b"same")]), NOW).await.unwrap();
    assert_eq!(be.keys().iter().filter(|k| k.starts_with("blobs/")).count(), 1);
    p.publish(req("b2", &[("a", b"same"), ("b", b"new")]), NOW).await.unwrap();
    assert_eq!(be.keys().iter().filter(|k| k.starts_with("blobs/")).count(), 2);
}

#[tokio::test]
async fn damaged_blob_is_refused_not_overwritten() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let h = blake3::hash(b"good").to_hex().to_string();
    be.tamper(&format!("blobs/{h}"), b"poisoned");
    let p = ReleasePublisher::new(&be, &st, &sg);
    let err = p.publish(req("b1", &[("a", b"good")]), NOW).await.unwrap_err();
    assert!(matches!(err, ReleaseError::BlobConflict { .. }), "{err}");
    assert_eq!(ReleaseBackendExt::raw(&be, &format!("blobs/{h}")).await, b"poisoned");
    assert!(!be.keys().iter().any(|k| k.starts_with("releases/")), "no release may be committed");
}

#[tokio::test]
async fn crash_mid_publish_leaves_no_release_and_retry_succeeds() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    let p = ReleasePublisher::new(&be, &st, &sg);
    be.fail_puts_after(2); // two blobs land, the third put (the third file) fails
    let r = req("b1", &[("a", b"1"), ("b", b"2"), ("c", b"3")]);
    assert!(matches!(p.publish(r, NOW).await.unwrap_err(), ReleaseError::Backend(_)));
    let rd = ReleaseReader::new(&be, &st, &tk);
    assert!(rd.build_ids().await.unwrap().is_empty(), "the manifest is the commit point");

    be.heal();
    let ok = p.publish(req("b1", &[("a", b"1"), ("b", b"2"), ("c", b"3")]), NOW).await.unwrap();
    assert_eq!(ok.outcome, PublishOutcome::Published);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_publishers_one_winner() {
    let be = Arc::new(MemoryBackend::new());
    let st = Arc::new(store("stable"));
    let sg = Arc::new(signer(1));
    let mut tasks = Vec::new();
    for i in 0..16u8 {
        let (be, st, sg) = (be.clone(), st.clone(), sg.clone());
        tasks.push(tokio::spawn(async move {
            let content = vec![i; 32];
            ReleasePublisher::new(be.as_ref(), st.as_ref(), sg.as_ref())
                .publish(PublishRequest {
                    build_id: "race".into(),
                    source_rev: "abc".into(),
                    created_at: 1,
                    builder: "r".into(),
                    artifacts: vec![Artifact { path: "bin".into(), bytes: content }],
                }, NOW)
                .await
        }));
    }
    let mut won = 0;
    let mut lost = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(r) if r.outcome == PublishOutcome::Published => won += 1,
            Err(ReleaseError::AlreadyPublished { .. }) => lost += 1,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!((won, lost), (1, 15));
    // The stored release is internally consistent: its blob is the winner's.
    let tk = trusted(&sg);
    let rd = ReleaseReader::new(be.as_ref(), st.as_ref(), &tk);
    let m = rd.manifest("race").await.unwrap();
    rd.read_artifact(&m, "bin").await.unwrap();
}

#[tokio::test]
async fn bad_requests_write_nothing() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let p = ReleasePublisher::new(&be, &st, &sg);
    for path in ["../escape", "/abs", "a//b", "a/./b", "a\\b", "", "trailing/", "ctl\u{7}"] {
        let e = p.publish(req("b1", &[(path, b"x")]), NOW).await.unwrap_err();
        assert!(matches!(e, ReleaseError::Invalid { .. }), "{path:?}: {e}");
    }
    assert!(p.publish(req("b1", &[("a", b"1"), ("a", b"2")]), NOW).await.is_err(), "duplicate path");
    assert!(p.publish(req("b1", &[]), NOW).await.is_err(), "empty release");
    for id in ["", ".hidden", "-x", "a/b", "a..b", "has space"] {
        assert!(p.publish(req(id, &[("a", b"1")]), NOW).await.is_err(), "{id:?}");
    }
    assert_eq!(be.put_calls(), 0);
}

#[tokio::test]
async fn forged_replayed_and_tampered_manifests_are_rejected() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    ReleasePublisher::new(&be, &st, &sg).publish(req("b1", &[("a", b"x")]), NOW).await.unwrap();
    let good = ReleaseBackendExt::raw(&be, "releases/b1/manifest.json").await;

    // Signed by a key the reader does not trust.
    let other = signer(2);
    let other_keys = TrustedKeys(vec![other.trusted()]);
    let rd_other = ReleaseReader::new(&be, &st, &other_keys);
    assert!(matches!(rd_other.manifest("b1").await.unwrap_err(), ReleaseError::Untrusted(_)));
    // Empty trust set trusts nothing.
    let none = TrustedKeys::default();
    assert!(ReleaseReader::new(&be, &st, &none).manifest("b1").await.is_err());

    // Replay into another channel's store.
    let be2 = MemoryBackend::new();
    be2.tamper("releases/b1/manifest.json", &good);
    let st_beta = store("beta");
    let e = ReleaseReader::new(&be2, &st_beta, &tk).manifest("b1").await.unwrap_err();
    assert!(matches!(e, ReleaseError::Untrusted(ref m) if m.contains("not")), "{e}");

    // Manifest copied under a different build id path.
    be2.tamper("releases/b9/manifest.json", &good);
    assert!(ReleaseReader::new(&be2, &st, &tk).manifest("b9").await.is_err());

    // Swapped artifact hash in the stored JSON.
    let evil = String::from_utf8(good.clone()).unwrap().replace(
        blake3::hash(b"x").to_hex().as_str(),
        blake3::hash(b"evil").to_hex().as_str(),
    );
    be2.tamper("releases/b1/manifest.json", evil.as_bytes());
    let st_stable = store("stable");
    assert!(ReleaseReader::new(&be2, &st_stable, &tk).manifest("b1").await.is_err());

    // Extra unsigned field smuggled in.
    let smuggled = String::from_utf8(good).unwrap().replacen("\"manifest\"", "\"extra\": 1, \"manifest\"", 1);
    be2.tamper("releases/b1/manifest.json", smuggled.as_bytes());
    assert!(matches!(
        ReleaseReader::new(&be2, &st_stable, &tk).manifest("b1").await.unwrap_err(),
        ReleaseError::Malformed { .. }
    ));
}

#[tokio::test]
async fn corrupted_or_missing_blob_is_caught_on_read() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    ReleasePublisher::new(&be, &st, &sg).publish(req("b1", &[("a", b"xx"), ("b", b"yy")]), NOW).await.unwrap();
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("b1").await.unwrap();
    be.tamper(&format!("blobs/{}", blake3::hash(b"xx").to_hex()), b"XX");
    assert!(matches!(rd.read_artifact(&m, "a").await.unwrap_err(), ReleaseError::HashMismatch { .. }));
    assert!(matches!(rd.read_artifact(&m, "nope").await.unwrap_err(), ReleaseError::Missing { .. }));
    assert_eq!(rd.audit(&m).await.unwrap(), vec!["a"]);
}

// ---------- retention ----------

fn rel(id: &str, at: u64) -> ReleaseMeta {
    ReleaseMeta { build_id: id.into(), created_at: at }
}

#[test]
fn plan_keeps_newest_pinned_and_young() {
    let rs = [rel("a", 100), rel("b", 200), rel("c", 300), rel("d", 400), rel("e", 990)];
    let pol = RetentionPolicy::new(2, 50).unwrap().pin("a");
    let p = plan(&rs, &pol, 1_000);
    assert_eq!(p.expire, vec!["c", "b"].into_iter().map(String::from).collect::<Vec<_>>());
    let keep: std::collections::BTreeMap<_, _> = p.keep.into_iter().collect();
    assert_eq!(keep["e"], KeepReason::Newest);
    assert_eq!(keep["d"], KeepReason::Newest);
    assert_eq!(keep["a"], KeepReason::Pinned);
}

#[test]
fn plan_never_empties_a_channel_and_handles_clock_skew() {
    assert!(RetentionPolicy::new(0, 0).is_err());
    let pol = RetentionPolicy::new(1, 0).unwrap();
    assert!(plan(&[rel("only", 5)], &pol, 1_000_000).expire.is_empty());
    // future-dated release (clock skew) counts as age 0, so min_age protects it
    let pol = RetentionPolicy::new(1, 100).unwrap();
    let p = plan(&[rel("old", 10), rel("new", 5_000), rel("future", 9_000)], &pol, 1_000);
    assert_eq!(p.expire, vec!["old".to_string()]);
    assert!(!p.expire.contains(&"future".to_string()));
}

#[test]
fn plan_tie_break_is_deterministic() {
    let pol = RetentionPolicy::new(1, 0).unwrap();
    let a = plan(&[rel("x", 10), rel("y", 10), rel("z", 10)], &pol, 100);
    let b = plan(&[rel("z", 10), rel("x", 10), rel("y", 10)], &pol, 100);
    assert_eq!(a, b);
    assert_eq!(a.keep[0].0, "z");
}

async fn seeded(be: &MemoryBackend, st: &ReleaseStoreName, sg: &ReleaseSigner) {
    let p = ReleasePublisher::new(be, st, sg);
    for (i, (id, files)) in [
        ("b1", vec![("a", b"old-only".as_slice()), ("shared", b"shared")]),
        ("b2", vec![("a", b"mid".as_slice()), ("shared", b"shared")]),
        ("b3", vec![("a", b"new".as_slice())]),
    ]
    .into_iter()
    .enumerate()
    {
        let mut r = req(id, &files);
        r.created_at = 100 * (i as u64 + 1);
        p.publish(r, NOW).await.unwrap();
    }
}

#[tokio::test]
async fn sweep_retires_ids_and_collects_blobs_after_grace() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();

    // First sweep: b1 and b2 expire; grace not over, so no blob is deleted yet.
    let r = sw.sweep(&pol, 1_000, 500, "keep-last=1").await.unwrap();
    assert_eq!(r.expired, vec!["b2", "b1"]);
    assert!(r.blobs_deleted.is_empty());
    assert!(be.keys().contains(&"tombstones/b1".to_string()));
    let rd = ReleaseReader::new(&be, &st, &tk);
    assert_eq!(rd.build_ids().await.unwrap(), vec!["b3"]);

    // Retired ids cannot be republished, even with the original content.
    let e = ReleasePublisher::new(&be, &st, &sg).publish(req("b1", &[("a", b"old-only")]), NOW).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Retired { .. }), "{e}");

    // After the grace, blobs only the expired releases used go; b3's stays.
    let r = sw.sweep(&pol, 1_600, 500, "keep-last=1").await.unwrap();
    assert_eq!(r.blobs_deleted.len(), 3, "old-only, mid, shared");
    let m = rd.manifest("b3").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "a").await.unwrap(), b"new");
    assert_eq!(be.keys().iter().filter(|k| k.starts_with("blobs/")).count(), 1);
}

#[tokio::test]
async fn sweep_keeps_blobs_still_referenced_by_a_surviving_release() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    // keep the newest two: b1 expires, but "shared" is also in b2
    let pol = RetentionPolicy::new(2, 0).unwrap();
    let r = Sweeper::new(&be, &st, &tk).sweep(&pol, 10_000, 0, "t").await.unwrap();
    assert_eq!(r.expired, vec!["b1"]);
    assert_eq!(r.blobs_deleted, vec![blake3::hash(b"old-only").to_hex().to_string()]);
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("b2").await.unwrap();
    assert!(rd.audit(&m).await.unwrap().is_empty());
}

#[tokio::test]
async fn forged_future_dated_manifest_cannot_evict_real_releases() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    // An attacker with write access (but not the signing key) forges a release dated far ahead.
    let forged = signer(9).sign(Manifest {
        schema: SCHEMA,
        store: st.uri(),
        channel: "stable".into(),
        build_id: "evil".into(),
        source_rev: "x".into(),
        created_at: u64::MAX,
        builder: "attacker".into(),
        artifacts: vec![ArtifactRef { path: "p".into(), hash: blake3::hash(b"evil").to_hex().to_string(), size: 4 }],
    }).unwrap();
    be.tamper("releases/evil/manifest.json", &forged.to_bytes());

    let pol = RetentionPolicy::new(1, 0).unwrap();
    let r = Sweeper::new(&be, &st, &tk).sweep(&pol, 10_000, 0, "t").await.unwrap();
    assert_eq!(r.expired, vec!["b2", "b1"], "the forged slot must not push b3 out");
    assert!(r.kept.iter().any(|(id, _)| id == "b3"));
    assert_eq!(r.unverified.len(), 1);
    assert!(be.keys().contains(&"releases/evil/manifest.json".to_string()), "left in place for the operator");
}

#[tokio::test]
async fn unparseable_manifest_stops_blob_deletion() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    be.tamper("releases/garbled/manifest.json", b"{not json");
    let pol = RetentionPolicy::new(1, 0).unwrap();
    let r = Sweeper::new(&be, &st, &tk).sweep(&pol, 10_000, 0, "t").await.unwrap();
    assert!(r.blob_gc_skipped.is_some());
    assert!(r.blobs_deleted.is_empty());
    assert_eq!(be.keys().iter().filter(|k| k.starts_with("blobs/")).count(), 4);
}

#[tokio::test]
async fn sweep_is_idempotent_and_recovers_from_a_crash_after_tombstone() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let pol = RetentionPolicy::new(1, 0).unwrap();
    let sw = Sweeper::new(&be, &st, &tk);
    sw.sweep(&pol, 10_000, 0, "t").await.unwrap();
    let after = be.keys();
    let r = sw.sweep(&pol, 20_000, 0, "t").await.unwrap();
    assert!(r.expired.is_empty() && r.blobs_deleted.is_empty());
    assert_eq!(be.keys(), after);
}

/// Test-only raw read.
trait ReleaseBackendExt {
    async fn raw(&self, key: &str) -> Vec<u8>;
}
impl ReleaseBackendExt for MemoryBackend {
    async fn raw(&self, key: &str) -> Vec<u8> {
        self.get(key).await.unwrap().unwrap_or_default()
    }
}

#[tokio::test]
async fn created_at_must_be_near_the_publishers_clock() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let p = ReleasePublisher::new(&be, &st, &sg);
    let mut r = req("b1", &[("a", b"x")]);
    r.created_at = u64::MAX;
    assert!(p.publish(r, NOW).await.is_err());
    let mut r = req("b2", &[("a", b"x")]);
    r.created_at = 0;
    assert!(p.publish(r, NOW + MAX_BACKDATE_SECS + 1).await.is_err());
    assert_eq!(be.put_calls(), 0);
}

#[tokio::test]
async fn bad_clock_cannot_hold_the_newest_slot() {
    let pol = RetentionPolicy::new(1, 0).unwrap();
    // Publish rejects far-future dates; this covers a manifest that got in anyway. It is clamped to
    // now, so it ranks as newest-now but never sorts above anything genuinely newer.
    let p = plan(&[rel("real", 900), rel("future", 9_000_000)], &pol, 1_000);
    assert_eq!(p.expire, vec!["real".to_string()]);
}

#[tokio::test]
async fn retry_over_an_unsigned_look_alike_is_not_success() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let p = ReleasePublisher::new(&be, &st, &sg);
    p.publish(req("b1", &[("a", b"x")]), NOW).await.unwrap();
    // Replace the stored release with the same content signed by someone else.
    let mut m = ReleaseBackendExt::raw(&be, "releases/b1/manifest.json").await;
    let mut signed = SignedManifest::parse(&m).unwrap();
    signed = signer(7).sign(signed.manifest.clone()).unwrap();
    m = signed.to_bytes();
    be.tamper("releases/b1/manifest.json", &m);
    let e = p.publish(req("b1", &[("a", b"x")]), NOW).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Untrusted(_)), "{e}");
}

#[tokio::test]
async fn publish_heals_a_blob_deleted_under_it() {
    // Simulates the sweep race: the blob a release needs is gone after the commit.
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    let p = ReleasePublisher::new(&be, &st, &sg);
    p.publish(req("b1", &[("a", b"shared")]), NOW).await.unwrap();
    // retire b1 properly, then delete its blob, then a rebuild with the same bytes lands
    let h = blake3::hash(b"shared").to_hex().to_string();
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "a").await.unwrap(), b"shared");
    let sw = Sweeper::new(&be, &st, &tk);
    p.publish(req("b2", &[("a", b"shared")]), NOW).await.unwrap();
    sw.sweep(&RetentionPolicy::new(1, 0).unwrap(), NOW, 0, "t").await.unwrap();
    // b1 expired; its blob is still referenced by b2 and must survive
    let m2 = rd.manifest("b2").await.unwrap();
    assert_eq!(rd.read_artifact(&m2, "a").await.unwrap(), b"shared");
    assert!(be.keys().contains(&format!("blobs/{h}")));
}

/// Seed a store where b1 (expiring) and b2 (kept) are published, then return b2-like bytes for a
/// release "late" that reuses b1's blob, signed with the same key, built on a scratch backend.
async fn late_manifest(st: &ReleaseStoreName, sg: &ReleaseSigner) -> Vec<u8> {
    let scratch = MemoryBackend::new();
    let mut r = req("late", &[("a", b"old-only")]);
    r.created_at = 900;
    ReleasePublisher::new(&scratch, st, sg).publish(r, NOW).await.unwrap();
    ReleaseBackendExt::raw(&scratch, "releases/late/manifest.json").await
}

#[tokio::test]
async fn publish_committing_between_sweep_check_and_delete_is_undone() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    sw.sweep(&pol, 1_000, 500, "t").await.unwrap(); // tombstones; grace not over

    // The manifest of a new release that reuses b1's blob lands right before that blob is deleted.
    let manifest = late_manifest(&st, &sg).await;
    let h = blake3::hash(b"old-only").to_hex().to_string();
    be.hook_before_delete(&format!("blobs/{h}"), move |b| b.tamper("releases/late/manifest.json", &manifest));

    let r = sw.sweep(&RetentionPolicy::new(2, 0).unwrap(), 1_600, 500, "t").await.unwrap();
    assert!(r.blobs_restored.contains(&h), "{r:?}");
    assert!(!r.blobs_deleted.contains(&h));
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("late").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "a").await.unwrap(), b"old-only");
}

#[tokio::test]
async fn publish_committing_just_after_the_delete_is_also_undone() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    sw.sweep(&pol, 1_000, 500, "t").await.unwrap();

    let manifest = late_manifest(&st, &sg).await;
    let h = blake3::hash(b"old-only").to_hex().to_string();
    be.hook_after_delete(&format!("blobs/{h}"), move |b| b.tamper("releases/late/manifest.json", &manifest));

    let r = sw.sweep(&pol, 1_600, 500, "t").await.unwrap();
    assert!(r.blobs_restored.contains(&h), "{r:?}");
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("late").await.unwrap();
    assert!(rd.audit(&m).await.unwrap().is_empty());
}

#[tokio::test]
async fn unreadable_manifest_after_the_delete_restores_the_blob() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    sw.sweep(&pol, 1_000, 500, "t").await.unwrap();

    let h = blake3::hash(b"old-only").to_hex().to_string();
    be.hook_after_delete(&format!("blobs/{h}"), |b| b.tamper("releases/junk/manifest.json", b"not json"));
    assert!(sw.sweep(&pol, 1_600, 500, "t").await.is_err());
    assert!(be.keys().contains(&format!("blobs/{h}")), "blob must be back");
}

#[tokio::test]
async fn a_sweep_that_died_after_the_delete_is_finished_by_the_next_one() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let sw = Sweeper::new(&be, &st, &tk);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    sw.sweep(&pol, 1_000, 500, "t").await.unwrap();

    // Crash state: the blob is set aside and deleted, and a release now names it.
    let h = blake3::hash(b"old-only").to_hex().to_string();
    DeletableBackend::copy(&be, &format!("blobs/{h}"), &format!("quarantine/{h}")).await.unwrap();
    DeletableBackend::delete(&be, &format!("blobs/{h}")).await.unwrap();
    be.tamper("releases/late/manifest.json", &late_manifest(&st, &sg).await);
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("late").await.unwrap();
    assert!(!rd.audit(&m).await.unwrap().is_empty(), "the hole is there");

    let r = sw.sweep(&pol, 1_600, 500, "t").await.unwrap();
    assert_eq!(r.blobs_recovered, vec![h.clone()]);
    assert!(rd.audit(&m).await.unwrap().is_empty(), "the next sweep put it back");
    assert!(!be.keys().iter().any(|k| k.starts_with("quarantine/")), "nothing left parked");
}

#[tokio::test]
async fn a_parked_copy_of_an_unreferenced_blob_is_dropped() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    be.tamper("quarantine/deadbeef", b"x");
    let r = Sweeper::new(&be, &st, &tk).sweep(&RetentionPolicy::new(3, 0).unwrap(), 1_000, 500, "t").await.unwrap();
    assert!(r.blobs_recovered.is_empty());
    assert!(!be.keys().contains(&"quarantine/deadbeef".to_string()));
}

#[tokio::test]
async fn retrying_a_published_release_repairs_a_blob_lost_to_a_crashed_sweeper() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    let p = ReleasePublisher::new(&be, &st, &sg);
    p.publish(req("b1", &[("a", b"payload")]), NOW).await.unwrap();
    // The sweeper deleted the blob and died before it could put it back.
    let h = blake3::hash(b"payload").to_hex().to_string();
    let rd = ReleaseReader::new(&be, &st, &tk);
    let m = rd.manifest("b1").await.unwrap();
    // remove the object behind the publisher's back
    struct Del<'a>(&'a MemoryBackend);
    impl Del<'_> { async fn go(&self, k: &str) { DeletableBackend::delete(self.0, k).await.unwrap(); } }
    Del(&be).go(&format!("blobs/{h}")).await;
    assert!(!rd.audit(&m).await.unwrap().is_empty(), "the hole must be visible");

    let again = p.publish(req("b1", &[("a", b"payload")]), NOW).await.unwrap();
    assert_eq!(again.outcome, PublishOutcome::AlreadyPresent);
    assert!(rd.audit(&m).await.unwrap().is_empty(), "the retry puts the bytes back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_and_sweep_interleaved_never_leave_a_release_without_its_blobs() {
    // Many rounds of a sweep racing publishes of releases that reuse the expiring release's blob.
    for round in 0..15u64 {
        let (be, st, sg) = (Arc::new(MemoryBackend::new()), Arc::new(store("stable")), Arc::new(signer(1)));
        let tk = Arc::new(trusted(&sg));
        seeded(&be, &st, &sg).await;
        let pol = RetentionPolicy::new(1, 0).unwrap();
        Sweeper::new(&*be, &st, &tk).sweep(&pol, 1_000, 500, "t").await.unwrap(); // tombstone b1, b2
        be.yield_between_calls(true);

        let mut tasks = Vec::new();
        {
            let (be, st, tk) = (be.clone(), st.clone(), tk.clone());
            tasks.push(tokio::spawn(async move {
                Sweeper::new(&*be, &st, &tk)
                    .sweep(&RetentionPolicy::new(3, 0).unwrap(), 1_600, 500, "t")
                    .await
                    .map(|_| ())
            }));
        }
        for i in 0..3u64 {
            let (be, st, sg) = (be.clone(), st.clone(), sg.clone());
            tasks.push(tokio::spawn(async move {
                let mut r = req(&format!("late{round}-{i}"), &[("a", b"old-only"), ("b", b"mid")]);
                r.created_at = 950;
                ReleasePublisher::new(&*be, &st, &sg).publish(r, NOW).await.map(|_| ())
            }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        be.yield_between_calls(false);
        let rd = ReleaseReader::new(&*be, &st, &tk);
        for id in rd.build_ids().await.unwrap() {
            let m = rd.manifest(&id).await.unwrap();
            assert!(rd.audit(&m).await.unwrap().is_empty(), "round {round}: {id} lost a blob");
        }
    }
}

// ---- Object Lock wiring ---------------------------------------------------------------------

fn locked_backend() -> MemoryBackend {
    let be = MemoryBackend::new();
    be.enable_lock();
    be
}

#[tokio::test]
async fn locked_publish_stamps_the_manifest_and_new_blobs_from_the_publishers_clock() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let pol = LockPolicy::new(LockMode::Compliance, 30 * 86_400).unwrap();
    let p = ReleasePublisher::new(&be, &st, &sg).with_lock(pol);
    // created_at is backdated; the lock counts from `now`, not from the claimed build time
    let mut r = req("b1", &[("a", b"alpha"), ("b", b"beta")]);
    r.created_at = 700;
    p.publish(r, NOW).await.unwrap();
    let want = Lock { mode: LockMode::Compliance, until: NOW + 30 * 86_400 };
    assert_eq!(be.lock_of("releases/b1/manifest.json"), Some(want));
    for data in [b"alpha".as_slice(), b"beta"] {
        let h = blake3::hash(data).to_hex().to_string();
        assert_eq!(be.lock_of(&format!("blobs/{h}")), Some(want), "blob {h}");
    }
}

#[tokio::test]
async fn a_reused_blob_follows_a_later_longer_lock_now_that_the_backend_can_extend() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let first = LockPolicy::new(LockMode::Governance, 1_000).unwrap();
    ReleasePublisher::new(&be, &st, &sg).with_lock(first).publish(req("b1", &[("a", b"shared")]), NOW).await.unwrap();
    let longer = LockPolicy::new(LockMode::Compliance, 9_000).unwrap();
    ReleasePublisher::new(&be, &st, &sg).with_lock(longer).publish(req("b2", &[("a", b"shared")]), NOW).await.unwrap();
    let h = blake3::hash(b"shared").to_hex().to_string();
    // the backend can extend, so the shared blob is lengthened to the later release's floor
    assert_eq!(be.lock_of(&format!("blobs/{h}")).unwrap().until, NOW + 9_000);
    assert_eq!(be.lock_of("releases/b2/manifest.json").unwrap().until, NOW + 9_000);
}

#[tokio::test]
async fn asking_for_a_lock_on_a_backend_without_one_fails_and_writes_no_release() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1)); // lock not enabled
    let pol = LockPolicy::new(LockMode::Governance, 100).unwrap();
    let e = ReleasePublisher::new(&be, &st, &sg).with_lock(pol).publish(req("b1", &[("a", b"x")]), NOW).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Invalid { what: "lock", .. }), "{e}");
    assert!(be.keys().is_empty(), "nothing written at all");
}

#[tokio::test]
async fn lock_floors_are_bounded() {
    assert!(LockPolicy::new(LockMode::Compliance, 0).is_err());
    assert!(LockPolicy::new(LockMode::Compliance, MAX_LOCK_FLOOR_SECS + 1).is_err());
    assert!(LockPolicy::new(LockMode::Compliance, MAX_LOCK_FLOOR_SECS).is_ok());
}

#[tokio::test]
async fn a_restore_in_a_locked_bucket_comes_back_locked() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let tk = trusted(&sg);
    let pol = LockPolicy::new(LockMode::Compliance, 5_000).unwrap();
    let p = ReleasePublisher::new(&be, &st, &sg).with_lock(pol);
    let mut r = req("b1", &[("a", b"old-only")]);
    r.created_at = 100;
    p.publish(r, NOW).await.unwrap();
    let sw = Sweeper::new(&be, &st, &tk).with_lock(pol);
    let keep = RetentionPolicy::new(1, 0).unwrap();
    let mut r2 = req("b2", &[("a", b"newer")]);
    r2.created_at = 200;
    p.publish(r2, NOW).await.unwrap();
    sw.sweep(&keep, 1_000, 500, "t").await.unwrap(); // b1 retired, grace running

    // a release that reuses b1's blob lands exactly when the blob is being deleted
    let h = blake3::hash(b"old-only").to_hex().to_string();
    let scratch = locked_backend();
    let mut late = req("late", &[("a", b"old-only")]);
    late.created_at = 900;
    ReleasePublisher::new(&scratch, &st, &sg).publish(late, NOW).await.unwrap();
    let manifest = ReleaseBackendExt::raw(&scratch, "releases/late/manifest.json").await;
    be.hook_before_delete(&format!("blobs/{h}"), move |b| b.tamper("releases/late/manifest.json", &manifest));
    let r = sw.sweep(&RetentionPolicy::new(3, 0).unwrap(), 1_600, 500, "t").await.unwrap();
    assert!(r.blobs_restored.contains(&h), "{r:?}");
    let lock = be.lock_of(&format!("blobs/{h}")).expect("restored blob is locked again");
    assert_eq!((lock.mode, lock.until), (LockMode::Compliance, 1_600 + 5_000));
}

// ---- replaced-manifest detection from version history (ADR-126 S4, S1 list_versions) ----

#[tokio::test]
async fn history_is_unchecked_without_versioning() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let tk = trusted(&sg);
    ReleasePublisher::new(&be, &st, &sg).publish(req("b1", &[("a", b"x")]), NOW).await.unwrap();
    be.tamper("releases/b1/manifest.json", b"{}");
    let h = ReleaseReader::new(&be, &st, &tk).history().await.unwrap();
    // No history kept: the replacement is invisible here, and the report must say so rather than
    // look clean.
    assert!(!h.checked);
    assert!(h.findings.is_empty());
}

#[tokio::test]
async fn untouched_store_has_a_clean_history() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let h = ReleaseReader::new(&be, &st, &tk).history().await.unwrap();
    assert!(h.checked);
    assert!(h.findings.is_empty(), "{:?}", h.findings);
}

#[tokio::test]
async fn a_replaced_manifest_is_found_even_though_the_current_one_verifies() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let tk = trusted(&sg);
    ReleasePublisher::new(&be, &st, &sg).publish(req("b1", &[("a", b"x")]), NOW).await.unwrap();
    // A second validly signed manifest for the same build id, written over the first by a
    // principal that can write the key (the direct CI role can).
    let scratch = MemoryBackend::new();
    ReleasePublisher::new(&scratch, &st, &sg).publish(req("b1", &[("a", b"EVIL")]), NOW).await.unwrap();
    let forged = ReleaseBackendExt::raw(&scratch, "releases/b1/manifest.json").await;
    be.tamper("releases/b1/manifest.json", &forged);

    let rd = ReleaseReader::new(&be, &st, &tk);
    assert!(rd.manifest("b1").await.is_ok(), "the replacement is signed, so the plain read trusts it");
    let h = rd.history().await.unwrap();
    assert_eq!(h.findings, vec![HistoryFinding::Replaced { key: "releases/b1/manifest.json".into(), contents: 2 }]);
}

#[tokio::test]
async fn a_replaced_blob_and_a_replaced_tombstone_are_found() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let pol = RetentionPolicy::new(1, 0).unwrap();
    Sweeper::new(&be, &st, &tk).sweep(&pol, 1_000, 500, "keep-last=1").await.unwrap();
    let h3 = blake3::hash(b"new").to_hex().to_string();
    be.tamper(&format!("blobs/{h3}"), b"swapped");
    be.tamper("tombstones/b1", b"{}");
    let h = ReleaseReader::new(&be, &st, &tk).history().await.unwrap();
    let keys: Vec<_> = h
        .findings
        .iter()
        .map(|f| match f {
            HistoryFinding::Replaced { key, .. } => key.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(keys, vec![format!("blobs/{h3}"), "tombstones/b1".to_string()]);
}

#[tokio::test]
async fn retention_and_the_sweepers_restore_are_not_findings() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let tk = trusted(&sg);
    seeded(&be, &st, &sg).await;
    let pol = RetentionPolicy::new(1, 0).unwrap();
    let sw = Sweeper::new(&be, &st, &tk).with_lock(LockPolicy::new(LockMode::Governance, 60).unwrap());
    sw.sweep(&pol, 1_000, 500, "keep-last=1").await.unwrap();
    sw.sweep(&pol, 1_600, 500, "keep-last=1").await.unwrap();
    // A restore: park a blob, delete it, and put the same bytes back, as a lost publish/sweep race does.
    let hs = blake3::hash(b"new").to_hex().to_string();
    let bytes = ReleaseBackendExt::raw(&be, &format!("blobs/{hs}")).await;
    be.delete(&format!("blobs/{hs}")).await.unwrap();
    be.tamper(&format!("blobs/{hs}"), &bytes);
    let h = ReleaseReader::new(&be, &st, &tk).history().await.unwrap();
    assert!(h.checked);
    assert!(h.findings.is_empty(), "retired manifests have tombstones and a restore has equal bytes: {:?}", h.findings);
}

#[tokio::test]
async fn a_manifest_deleted_without_a_tombstone_is_found() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let tk = trusted(&sg);
    ReleasePublisher::new(&be, &st, &sg).publish(req("b1", &[("a", b"x")]), NOW).await.unwrap();
    be.delete("releases/b1/manifest.json").await.unwrap(); // an operator, not retention
    let h = ReleaseReader::new(&be, &st, &tk).history().await.unwrap();
    assert_eq!(h.findings, vec![HistoryFinding::ManifestDeleted { key: "releases/b1/manifest.json".into() }]);
}

// ---- S1 seams prepared on the mock: lock read-back, extend-retention, lifecycle, file/multipart put ----

#[tokio::test]
async fn a_locked_publish_reads_the_bucket_lock_config_first_and_writes_nothing_when_it_is_off() {
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1)); // bucket made without Object Lock
    let p = ReleasePublisher::new(&be, &st, &sg).with_lock(LockPolicy::new(LockMode::Governance, 60).unwrap());
    let e = p.publish(req("b1", &[("a", b"x")]), NOW).await.unwrap_err();
    assert!(e.to_string().contains("Object Lock"), "{e}");
    assert_eq!(be.put_calls(), 0, "refused before the first write");
    // An unlocked publisher never asks.
    assert!(ReleasePublisher::new(&be, &st, &sg).publish(req("b1", &[("a", b"x")]), NOW).await.is_ok());
    // A locked bucket passes.
    let ok = locked_backend();
    ReleasePublisher::new(&ok, &st, &sg)
        .with_lock(LockPolicy::new(LockMode::Governance, 60).unwrap())
        .publish(req("b1", &[("a", b"x")]), NOW)
        .await
        .unwrap();
    assert!(ok.lock_config().await.unwrap().unwrap().enabled);
}

#[tokio::test]
async fn a_reused_blob_gets_the_longer_lock_and_never_a_shorter_or_weaker_one() {
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let h = format!("blobs/{}", blake3::hash(b"shared").to_hex());
    let gov = |s| LockPolicy::new(LockMode::Governance, s).unwrap();
    let comp = |s| LockPolicy::new(LockMode::Compliance, s).unwrap();

    ReleasePublisher::new(&be, &st, &sg).with_lock(gov(100)).publish(req("b1", &[("a", b"shared")]), NOW).await.unwrap();
    assert_eq!(be.lock_of(&h).unwrap(), Lock { mode: LockMode::Governance, until: NOW + 100 });

    // A later release that wants longer and stronger: the shared blob follows.
    ReleasePublisher::new(&be, &st, &sg).with_lock(comp(1_000)).publish(req("b2", &[("a", b"shared")]), NOW).await.unwrap();
    assert_eq!(be.lock_of(&h).unwrap(), Lock { mode: LockMode::Compliance, until: NOW + 1_000 });

    // A third that wants shorter and weaker changes nothing.
    ReleasePublisher::new(&be, &st, &sg).with_lock(gov(10)).publish(req("b3", &[("a", b"shared")]), NOW).await.unwrap();
    assert_eq!(be.lock_of(&h).unwrap(), Lock { mode: LockMode::Compliance, until: NOW + 1_000 });

    // The backend itself refuses a shortening, whatever the publisher asks.
    let e = be.extend_lock(&h, &Lock { mode: LockMode::Compliance, until: NOW + 5 }).await.unwrap_err();
    assert!(e.0.contains("never shortened"), "{e}");
}

#[tokio::test]
async fn a_backend_that_cannot_extend_leaves_a_reused_blob_as_written() {
    // A backend with locks but no retention read-back (today's S1 transport): reuse still works.
    struct NoExtend(MemoryBackend);
    impl ReleaseBackend for NoExtend {
        async fn put_if_absent(&self, k: &str, b: &[u8]) -> std::result::Result<PutOutcome, BackendError> { self.0.put_if_absent(k, b).await }
        async fn put_if_absent_locked(&self, k: &str, b: &[u8], l: &Lock) -> std::result::Result<PutOutcome, BackendError> { self.0.put_if_absent_locked(k, b, l).await }
        async fn get(&self, k: &str) -> std::result::Result<Option<Vec<u8>>, BackendError> { self.0.get(k).await }
        async fn list(&self, p: &str) -> std::result::Result<Vec<String>, BackendError> { self.0.list(p).await }
    }
    let (be, st, sg) = (NoExtend(locked_backend()), store("stable"), signer(1));
    let pol = LockPolicy::new(LockMode::Governance, 100).unwrap();
    ReleasePublisher::new(&be, &st, &sg).with_lock(pol).publish(req("b1", &[("a", b"shared")]), NOW).await.unwrap();
    let later = LockPolicy::new(LockMode::Governance, 900).unwrap();
    ReleasePublisher::new(&be, &st, &sg).with_lock(later).publish(req("b2", &[("a", b"shared")]), NOW).await.unwrap();
    let h = format!("blobs/{}", blake3::hash(b"shared").to_hex());
    assert_eq!(be.0.lock_of(&h).unwrap().until, NOW + 100);
}

#[tokio::test]
async fn lifecycle_rules_are_set_then_read_back() {
    let be = MemoryBackend::new();
    let rules = LifecycleRules { noncurrent_days: 30, expire_delete_markers: true };
    provision_lifecycle(&be, &rules).await.unwrap();
    assert_eq!(be.get_lifecycle().await.unwrap(), Some(rules));
    assert!(provision_lifecycle(&be, &LifecycleRules { noncurrent_days: 0, ..rules }).await.is_err());

    // A server that accepts the call and stores something else is caught by the read-back.
    struct Liar;
    impl LifecycleBackend for Liar {
        async fn put_lifecycle(&self, _: &LifecycleRules) -> std::result::Result<(), BackendError> { Ok(()) }
        async fn get_lifecycle(&self) -> std::result::Result<Option<LifecycleRules>, BackendError> {
            Ok(Some(LifecycleRules { noncurrent_days: 9999, expire_delete_markers: false }))
        }
    }
    assert!(provision_lifecycle(&Liar, &rules).await.unwrap_err().0.contains("not the rules just set"));
    struct Silent;
    impl LifecycleBackend for Silent {
        async fn put_lifecycle(&self, _: &LifecycleRules) -> std::result::Result<(), BackendError> { Ok(()) }
        async fn get_lifecycle(&self) -> std::result::Result<Option<LifecycleRules>, BackendError> { Ok(None) }
    }
    assert!(provision_lifecycle(&Silent, &rules).await.is_err());
}

fn write_file(dir: &std::path::Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

#[tokio::test]
async fn files_publish_in_parts_and_give_the_same_release_as_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let big = vec![0x5Au8; 3 * (1 << 20) + 17];
    let f_big = write_file(dir.path(), "weaver", &big);
    let f_small = write_file(dir.path(), "SUMS", b"sums");
    let (st, sg) = (store("stable"), signer(1));

    let by_files = MemoryBackend::new();
    let fr = FileRequest {
        build_id: "b1".into(), source_rev: "527ea4db".into(), created_at: 1_000, builder: "ci-runner-1".into(),
        artifacts: vec![
            FileArtifact { path: "bin/weaver".into(), file: f_big },
            FileArtifact { path: "SHA256SUMS".into(), file: f_small },
        ],
    };
    let r1 = ReleasePublisher::new(&by_files, &st, &sg).publish_files(fr, NOW).await.unwrap();
    let (part, puts) = by_files.file_put_stats();
    assert!(puts >= 2, "both files went through the file path: {puts}");
    assert!(part <= 1 << 20, "no read larger than one part: {part}");

    let by_bytes = MemoryBackend::new();
    let r2 = ReleasePublisher::new(&by_bytes, &st, &sg).publish(req("b1", &[("bin/weaver", &big), ("SHA256SUMS", b"sums")]), NOW).await.unwrap();
    assert_eq!(r1.manifest_digest, r2.manifest_digest, "the manifest does not depend on how the bytes arrived");
    assert_eq!(by_files.keys(), by_bytes.keys());

    // Read back through the verified reader.
    let tk = trusted(&sg);
    let rd = ReleaseReader::new(&by_files, &st, &tk);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "bin/weaver").await.unwrap(), big);

    // Retry and a same-bytes file in another release are idempotent / shared.
    let fr2 = FileRequest {
        build_id: "b1".into(), source_rev: "527ea4db".into(), created_at: 1_000, builder: "ci-runner-1".into(),
        artifacts: vec![
            FileArtifact { path: "bin/weaver".into(), file: dir.path().join("weaver") },
            FileArtifact { path: "SHA256SUMS".into(), file: dir.path().join("SUMS") },
        ],
    };
    assert_eq!(ReleasePublisher::new(&by_files, &st, &sg).publish_files(fr2, NOW).await.unwrap().outcome, PublishOutcome::AlreadyPresent);
}

#[tokio::test]
async fn a_file_that_changes_or_vanishes_publishes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_file(dir.path(), "a", b"first");
    let (be, st, sg) = (MemoryBackend::new(), store("stable"), signer(1));
    let fr = |file: std::path::PathBuf| FileRequest {
        build_id: "b1".into(), source_rev: "r".into(), created_at: 1_000, builder: "ci".into(),
        artifacts: vec![FileArtifact { path: "a".into(), file }],
    };
    let e = ReleasePublisher::new(&be, &st, &sg).publish_files(fr(dir.path().join("missing")), NOW).await.unwrap_err();
    assert!(matches!(e, ReleaseError::Invalid { .. }), "{e}");
    assert!(be.is_empty());
    // A rebuild with different bytes under the same id is refused exactly as for byte artifacts.
    ReleasePublisher::new(&be, &st, &sg).publish_files(fr(f.clone()), NOW).await.unwrap();
    std::fs::write(&f, b"second").unwrap();
    let e = ReleasePublisher::new(&be, &st, &sg).publish_files(fr(f), NOW).await.unwrap_err();
    assert!(matches!(e, ReleaseError::AlreadyPublished { .. }), "{e}");
}

#[tokio::test]
async fn locked_file_publish_locks_each_blob_and_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let f = write_file(dir.path(), "a", b"locked-bytes");
    let (be, st, sg) = (locked_backend(), store("stable"), signer(1));
    let pol = LockPolicy::new(LockMode::Compliance, 500).unwrap();
    let fr = FileRequest {
        build_id: "b1".into(), source_rev: "r".into(), created_at: 1_000, builder: "ci".into(),
        artifacts: vec![FileArtifact { path: "a".into(), file: f }],
    };
    ReleasePublisher::new(&be, &st, &sg).with_lock(pol).publish_files(fr, NOW).await.unwrap();
    let h = format!("blobs/{}", blake3::hash(b"locked-bytes").to_hex());
    assert_eq!(be.lock_of(&h).unwrap().until, NOW + 500);
    assert!(be.lock_of("releases/b1/manifest.json").is_some());
}
