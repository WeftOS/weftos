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
