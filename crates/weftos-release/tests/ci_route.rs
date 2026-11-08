//! The two CI routes (daemon-mediated append, direct role) behind one switch, tested both ways.
#![cfg(feature = "store")]

use std::sync::Arc;

use weftos_pq_sign::MlDsa65SigningKey;
use weftos_release::route::{ci_backend, CiBackend, CiRoute, ServiceBackend, DEFAULT_CI_ROUTE};
use weftos_release::s3::StoreBackend;
use weftos_release::*;
use weftos_store::audit::MemSink;
use weftos_store::service::{Caller, Config, NoChain, OpenGate, Parties};
use weftos_store::{Access, AdminBackend, MemBackend, ObjectBackend, Principal, StoreName, StoreService};

const ULID: &str = "01JBXZ3N5C8QW4M2R7T9V6YH0K";

fn signer() -> ReleaseSigner {
    ReleaseSigner::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32]), MlDsa65SigningKey::from_seed(&[7; 32]))
}

fn rq(build: &str, at: u64, files: &[(&str, &[u8])]) -> PublishRequest {
    PublishRequest {
        build_id: build.into(),
        source_rev: "abc".into(),
        created_at: at,
        builder: "ci".into(),
        artifacts: files.iter().map(|(p, b)| Artifact { path: (*p).into(), bytes: b.to_vec() }).collect(),
    }
}

static NAME_CELL: std::sync::OnceLock<ReleaseStoreName> = std::sync::OnceLock::new();
#[allow(non_snake_case)]
fn NAME_get() -> &'static ReleaseStoreName {
    NAME_CELL.get_or_init(st)
}

fn st() -> ReleaseStoreName {
    ReleaseStoreName::new("pg-mesh", ULID, "stable").unwrap()
}

fn who(id: &str) -> Principal {
    Principal::parse(&format!("cog:{id}")).unwrap()
}

struct Rig {
    svc: Arc<StoreService>,
    mem: Arc<MemBackend>,
    sink: Arc<MemSink>,
    name: StoreName,
}

fn rig() -> Rig {
    let mem = Arc::new(MemBackend::new());
    let sink = Arc::new(MemSink::default());
    let svc = Arc::new(
        StoreService::open(
            Config { node_id: "pg-1".into(), state_path: None, clock: || 1_000 },
            mem.clone(),
            mem.clone(),
            Arc::new(OpenGate),
            sink.clone(),
            Arc::new(NoChain),
        )
        .unwrap(),
    );
    let name = StoreName::parse(&st().uri()).unwrap();
    svc.create_store(&name, "daemon", Parties::default()).unwrap();
    Rig { svc, mem, sink, name }
}

fn service_as(r: &Rig, id: &str, access: Access) -> ServiceBackend {
    r.svc.grant(&r.name, who(id), access, "op").unwrap();
    ServiceBackend::new(r.svc.clone(), Caller::local(who(id)), &st()).unwrap()
}

#[test]
fn the_switch_is_one_line_and_parses_both_ways() {
    assert_eq!(CiRoute::default(), DEFAULT_CI_ROUTE);
    assert_eq!(CiRoute::parse("daemon").unwrap(), CiRoute::Daemon);
    assert_eq!(CiRoute::parse("direct").unwrap(), CiRoute::Direct);
    assert!(CiRoute::parse("both").is_err());
    assert_eq!(CiRoute::Direct.as_str(), "direct");
}

#[test]
fn a_route_given_the_other_routes_inputs_is_refused() {
    let r = rig();
    let svc_half = Some((r.svc.clone(), Caller::local(who("ci"))));
    let none_direct: Option<Arc<MemBackend>> = None;
    assert!(ci_backend::<MemBackend>(CiRoute::Direct, &st(), svc_half.clone(), none_direct.clone()).is_err());
    assert!(ci_backend::<MemBackend>(CiRoute::Daemon, &st(), None, Some(r.mem.clone())).is_err());
    assert_eq!(ci_backend::<MemBackend>(CiRoute::Daemon, &st(), svc_half, None).unwrap().route(), CiRoute::Daemon);
    r.mem.create_bucket(&StoreName::parse(&st().uri()).unwrap().bucket(), &[]).ok();
    assert_eq!(ci_backend::<MemBackend>(CiRoute::Direct, &st(), None, Some(r.mem.clone())).unwrap().route(), CiRoute::Direct);
}

/// The scenario every route must pass: publish, idempotent retry, refused rebuild, read back.
async fn scenario<B: ReleaseBackend>(be: &B) {
    let (st, sg) = (st(), signer());
    let tk = TrustedKeys(vec![sg.trusted()]);
    let p = ReleasePublisher::new(be, &st, &sg);
    let first = p.publish(rq("b1", 100, &[("bin/weaver", b"ELF")]), 1_000).await.unwrap();
    assert_eq!(first.outcome, PublishOutcome::Published);
    let again = p.publish(rq("b1", 100, &[("bin/weaver", b"ELF")]), 1_000).await.unwrap();
    assert_eq!(again.outcome, PublishOutcome::AlreadyPresent);
    let e = p.publish(rq("b1", 100, &[("bin/weaver", b"REBUILT")]), 1_000).await.unwrap_err();
    assert!(matches!(e, ReleaseError::AlreadyPublished { .. }), "{e}");
    let rd = ReleaseReader::new(be, &st, &tk);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "bin/weaver").await.unwrap(), b"ELF");
    assert!(rd.audit(&m).await.unwrap().is_empty());
}

#[tokio::test]
async fn daemon_route_publishes_as_an_append_principal() {
    let r = rig();
    let ci = service_as(&r, "ci", Access::Append);
    scenario(&ci).await;
    // The service saw it: puts by the CI principal are in the activity log.
    let puts = r.sink.events().iter().filter(|e| e.principal == "cog:ci" && e.op.contains("Put") || e.op == "put").count();
    assert!(puts >= 2, "blob and manifest puts are logged, got {puts}");
}

#[tokio::test]
async fn direct_route_publishes_with_the_same_result() {
    let mem = Arc::new(MemBackend::new());
    let bucket = StoreName::parse(&st().uri()).unwrap().bucket();
    mem.create_bucket(&bucket, &[]).unwrap();
    let be: CiBackend<MemBackend> = ci_backend(CiRoute::Direct, &st(), None, Some(mem.clone())).unwrap();
    scenario(&be).await;
}

#[tokio::test]
async fn only_the_daemon_route_stops_a_blind_overwrite() {
    // Daemon: an append principal that tries a plain put (no If-None-Match) is refused by the service.
    let r = rig();
    let ci = service_as(&r, "ci", Access::Append);
    ci.put_if_absent("releases/b1/manifest.json", b"first").await.unwrap();
    let blind = tokio::task::spawn_blocking({
        let (svc, name) = (r.svc.clone(), r.name.clone());
        move || svc.put(&Caller::local(who("ci")), &name, "releases/b1/manifest.json", b"second", false)
    })
    .await
    .unwrap();
    assert!(blind.is_err(), "append must not overwrite");
    assert_eq!(ci.get("releases/b1/manifest.json").await.unwrap().unwrap(), b"first");

    // Direct: the same bytes written with a plain put replace it. The key's own policy cannot say
    // otherwise (measured on RustFS, tests/store_ci_key.rs), which is why the route is a choice.
    let mem = Arc::new(MemBackend::new());
    let bucket = StoreName::parse(&st().uri()).unwrap().bucket();
    mem.create_bucket(&bucket, &[]).unwrap();
    let direct: StoreBackend<MemBackend> = StoreBackend::for_release(mem.clone(), &st()).unwrap();
    direct.put_if_absent("releases/b1/manifest.json", b"first").await.unwrap();
    mem.put(&bucket, "releases/b1/manifest.json", b"second", false).unwrap();
    assert_eq!(direct.get("releases/b1/manifest.json").await.unwrap().unwrap(), b"second");
}

#[tokio::test]
async fn a_read_principal_cannot_publish_on_either_route_and_a_stranger_cannot_read() {
    let r = rig();
    let reader = service_as(&r, "viewer", Access::Read);
    assert!(reader.put_if_absent("blobs/x", b"x").await.is_err());
    let stranger = ServiceBackend::new(r.svc.clone(), Caller::local(who("nobody")), &st()).unwrap();
    assert!(stranger.get("blobs/x").await.is_err());
    assert!(stranger.list("").await.is_err());
}

#[tokio::test]
async fn a_locked_publish_through_the_service_needs_a_locked_store() {
    let r = rig(); // made without Object Lock
    let ci = service_as(&r, "ci", Access::Append);
    let sg = signer();
    let p = ReleasePublisher::new(&ci, NAME_get(), &sg).with_lock(LockPolicy::new(LockMode::Governance, 60).unwrap());
    let e = p.publish(rq("b1", 100, &[("a", b"x")]), 1_000).await.unwrap_err();
    assert!(e.to_string().contains("object lock"), "{e}");
    assert!(r.mem.list(&StoreName::parse(&st().uri()).unwrap().bucket(), "").unwrap().is_empty(), "nothing unprotected was written");
    // No version history on an unlocked store: history() says it could not check.
    let tk = TrustedKeys(vec![sg.trusted()]);
    assert!(!ReleaseReader::new(&ci, &st(), &tk).history().await.unwrap().checked);
}

#[tokio::test]
async fn the_sweep_role_cannot_run_a_sweep_and_deletes_nothing() {
    // S1's `sweep` role is read + delete: no put and no copy. The quarantine copy is the first write
    // a sweep makes, so it is refused before any delete.
    let r = rig();
    let ci = service_as(&r, "ci", Access::Append);
    let sg = signer();
    let tk = TrustedKeys(vec![sg.trusted()]);
    let p = ReleasePublisher::new(&ci, NAME_get(), &sg);
    for (i, (id, body)) in [("b1", b"old".as_slice()), ("b2", b"new".as_slice())].into_iter().enumerate() {
        p.publish(rq(id, 100 * (i as u64 + 1), &[("a", body)]), 1_000).await.unwrap();
    }
    let sweeper = service_as(&r, "sweeper", Access::Sweep);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    // The first write a sweep makes is b1's tombstone: refused under `sweep` (no put), before any delete.
    let e = Sweeper::new(&sweeper, &st(), &tk).sweep(&pol, 1_000, 0, "keep-last=1").await.unwrap_err();
    assert!(e.to_string().contains("holds sweep") && e.to_string().contains("PutObject"), "{e}");
    let ids = ReleaseReader::new(&ci, &st(), &tk).build_ids().await.unwrap();
    assert_eq!(ids, vec!["b1", "b2"], "a refused sweep removed nothing");
}

#[tokio::test]
async fn the_write_role_runs_the_whole_sweep_including_quarantine() {
    let r = rig();
    let ci = service_as(&r, "ci", Access::Append);
    let sg = signer();
    let tk = TrustedKeys(vec![sg.trusted()]);
    let p = ReleasePublisher::new(&ci, NAME_get(), &sg);
    for (i, (id, body)) in [("b1", b"old".as_slice()), ("b2", b"new".as_slice())].into_iter().enumerate() {
        p.publish(rq(id, 100 * (i as u64 + 1), &[("a", body)]), 1_000).await.unwrap();
    }
    let sweeper = service_as(&r, "sweeper", Access::Write);
    let pol = RetentionPolicy::new(1, 0).unwrap();
    let sw = Sweeper::new(&sweeper, NAME_get(), &tk);
    let rep = sw.sweep(&pol, 1_000, 0, "keep-last=1").await.unwrap();
    assert_eq!(rep.expired, vec!["b1"]);
    assert_eq!(rep.blobs_deleted.len(), 1, "the quarantine copy, delete and re-check all ran through the service");
    let rd = ReleaseReader::new(&ci, NAME_get(), &tk);
    assert_eq!(rd.build_ids().await.unwrap(), vec!["b2"]);
    let m = rd.manifest("b2").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "a").await.unwrap(), b"new");
}

/// The daemon route on a real, locked RustFS bucket: an `append` principal publishes locked through the
/// service, a `read` principal sees the version history, and a plain (non-conditional) put by the append
/// principal is refused by the service. Needs a THROWAWAY RustFS (`WEFTOS_RELEASE_TEST_RUSTFS_ENV`).
#[tokio::test]
#[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
async fn live_daemon_route_on_a_locked_store() {
    use weftos_store::{RustfsBackend, RustfsConfig};
    let f = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV").expect("WEFTOS_RELEASE_TEST_RUSTFS_ENV");
    let dir = tempfile::tempdir().unwrap();
    let cfg = RustfsConfig::from_env_file(std::path::Path::new(&f), dir.path().join("secrets")).unwrap();
    let svc = tokio::task::spawn_blocking(move || {
        let be = Arc::new(RustfsBackend::new(cfg).unwrap());
        let svc = Arc::new(
            StoreService::open(
                Config { node_id: "pg-1".into(), state_path: None, clock: || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 },
                be.clone(),
                be.clone(),
                Arc::new(OpenGate),
                Arc::new(MemSink::default()),
                Arc::new(NoChain),
            )
            .unwrap(),
        );
        (svc, be)
    })
    .await
    .unwrap();
    let (svc, probe) = svc;
    let channel = format!("d{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
    let store = ReleaseStoreName::new("pg-mesh", ULID, &channel).unwrap();
    let name = StoreName::parse(&store.uri()).unwrap();
    {
        let (s, n) = (svc.clone(), name.clone());
        tokio::task::spawn_blocking(move || {
            s.create_locked_store(&n, "daemon", Parties::default()).unwrap();
            s.grant(&n, who("ci"), Access::Append, "op").unwrap();
            s.grant(&n, who("viewer"), Access::Read, "op").unwrap();
        })
        .await
        .unwrap();
    }
    let ci = ServiceBackend::new(svc.clone(), Caller::local(who("ci")), &store).unwrap();
    let viewer = ServiceBackend::new(svc.clone(), Caller::local(who("viewer")), &store).unwrap();
    let sg = signer();
    let tk = TrustedKeys(vec![sg.trusted()]);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let p = ReleasePublisher::new(&ci, &store, &sg).with_lock(LockPolicy::new(LockMode::Governance, 3_600).unwrap());
    p.publish(rq("b1", now - 10, &[("bin/weaver", b"ELF")]), now).await.unwrap();
    assert_eq!(p.publish(rq("b1", now - 10, &[("bin/weaver", b"ELF")]), now).await.unwrap().outcome, PublishOutcome::AlreadyPresent);
    let e = p.publish(rq("b1", now - 10, &[("bin/weaver", b"EVIL")]), now).await.unwrap_err();
    assert!(matches!(e, ReleaseError::AlreadyPublished { .. }), "{e}");

    let rd = ReleaseReader::new(&viewer, &store, &tk);
    let m = rd.manifest("b1").await.unwrap();
    assert_eq!(rd.read_artifact(&m, "bin/weaver").await.unwrap(), b"ELF");
    let h = rd.history().await.unwrap();
    assert!(h.checked && h.findings.is_empty(), "{h:?}");

    // a file larger than one part goes through the service's put_stream, locked, as the append principal
    assert!(ci.lock_config().await.unwrap().unwrap().enabled);
    let big: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i % 241) as u8).collect();
    let fpath = dir.path().join("big.bin");
    std::fs::write(&fpath, &big).unwrap();
    let fr = FileRequest {
        build_id: "b2".into(), source_rev: "r".into(), created_at: now - 5, builder: "ci".into(),
        artifacts: vec![FileArtifact { path: "bin/big".into(), file: fpath }],
    };
    p.publish_files(fr, now).await.unwrap();
    let m2 = rd.manifest("b2").await.unwrap();
    assert_eq!(rd.read_artifact(&m2, "bin/big").await.unwrap(), big);

    // S1's extend-retention on the service route: a later release that reuses the blob moves its date out
    // (asked blind, then read back at RustFS), a shorter one leaves it, and an append principal cannot
    // reach past S1's five-year cap.
    let blob = format!("blobs/{}", blake3::hash(b"ELF").to_hex());
    let held = |probe: Arc<weftos_store::RustfsBackend>, bucket: String, key: String| {
        tokio::task::spawn_blocking(move || {
            let v = probe.list_versions(&bucket, &key).unwrap().into_iter().find(|v| v.key == key && v.is_latest).unwrap();
            probe.get_retention(&bucket, &key, &v.version_id).unwrap().unwrap().until_unix_s
        })
    };
    let bucket = name.bucket();
    let before = held(probe.clone(), bucket.clone(), blob.clone()).await.unwrap();
    let longer = LockPolicy::new(LockMode::Governance, 7_200).unwrap();
    ReleasePublisher::new(&ci, &store, &sg).with_lock(longer).publish(rq("b3", now - 4, &[("bin/weaver", b"ELF")]), now).await.unwrap();
    let after = held(probe.clone(), bucket.clone(), blob.clone()).await.unwrap();
    assert!(after > before && after >= now + 7_200 - 5, "extended {before} -> {after}");
    let shorter = LockPolicy::new(LockMode::Governance, 60).unwrap();
    ReleasePublisher::new(&ci, &store, &sg).with_lock(shorter).publish(rq("b4", now - 3, &[("bin/weaver", b"ELF")]), now).await.unwrap();
    assert_eq!(held(probe.clone(), bucket, blob.clone()).await.unwrap(), after, "a shorter ask leaves it");
    let far = Lock { mode: LockMode::Governance, until: now + 6 * 365 * 86_400 };
    let e = ci.extend_lock(&blob, &far).await.unwrap_err();
    assert!(e.0.contains("at most"), "{e}");

    // the service refuses what RustFS alone could not
    let blind = {
        let (s, n) = (svc.clone(), name.clone());
        tokio::task::spawn_blocking(move || s.put(&Caller::local(who("ci")), &n, "releases/b1/manifest.json", b"x", false))
            .await
            .unwrap()
    };
    assert!(blind.is_err());
    // the viewer cannot write at all
    assert!(viewer.put_if_absent("blobs/x", b"x").await.is_err());
    tokio::task::spawn_blocking(move || drop((ci, viewer, svc, probe))).await.unwrap();
}

// ---- release archives: S1's archive_gc, reported as a dry run only ----

static ARCH_NOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static ARCH_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn arch_clock() -> u64 {
    ARCH_NOW.load(std::sync::atomic::Ordering::SeqCst)
}

#[test]
fn the_release_archive_report_is_a_dry_run_and_sets_no_number() {
    use weftos_release::route::release_archive_report;
    const DAY: u64 = 86_400_000;
    let _turn = ARCH_TURN.lock().unwrap_or_else(|e| e.into_inner());
    ARCH_NOW.store(10 * DAY, std::sync::atomic::Ordering::SeqCst);
    let mem = Arc::new(MemBackend::new());
    let svc = StoreService::open(
        Config { node_id: "pg-1".into(), state_path: None, clock: arch_clock },
        mem.clone(),
        mem.clone(),
        Arc::new(OpenGate),
        Arc::new(MemSink::default()),
        Arc::new(NoChain),
    )
    .unwrap();
    let name = StoreName::parse(&st().uri()).unwrap();
    svc.create_store(&name, "daemon", Parties::default()).unwrap();
    mem.put(&name.bucket(), "releases/b1/manifest.json", b"m", false).unwrap();
    mem.put(&name.bucket(), "blobs/aa", b"b", false).unwrap();
    // retire the release store: its objects move to the archive bucket
    assert_eq!(svc.archive(&name, false, "op").unwrap(), 2);
    ARCH_NOW.store(50 * DAY, std::sync::atomic::Ordering::SeqCst); // 40 days later

    // no number set: nothing is marked to expire, and it says it was a dry run
    let none = release_archive_report(&svc, None, "op").unwrap();
    assert!(none.dry_run && none.expired.is_empty() && none.deleted_objects == 0);
    assert_eq!(none.kept, 1);

    // a preview at 30 days names the release archive whole, deletes nothing, and leaves no chain record
    let prev = release_archive_report(&svc, Some(30), "op").unwrap();
    assert!(prev.dry_run);
    assert_eq!((prev.expired.len(), prev.expired[0].kind.as_str(), prev.expired[0].objects), (1, "release", 2));
    assert_eq!(prev.deleted_objects, 0);
    assert_eq!(mem.list(weftos_store::service::ARCHIVE_BUCKET, "").unwrap().len(), 2, "archive intact");

    // younger than the number: kept; zero days is refused, as S1 does
    assert_eq!(release_archive_report(&svc, Some(60), "op").unwrap().expired.len(), 0);
    assert!(release_archive_report(&svc, Some(0), "op").is_err());
}
