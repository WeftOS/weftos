//! Card 5e5b5d23, review round 2 (L-a handler side, L-c, N-r2-1): a principal's `store.worktree.*`
//! verbs go through the daemon's routing (`principal_route` then `run_routed`), the same two calls
//! `dispatch_after_auth` makes. Where `principal_route` answers `None`, the call falls to the
//! operator path with no actor, as `daemon.rs` does, so a verb that is not routed fails these tests.
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use clawft_kernel::exposure::registry::ExposureRegistry;
use serde_json::{Value, json};
use weftos_store::{Access, FsBackend, StoreName, StoreService};

use super::tests::{PROJ, WRITER_UID, caller, p};
use super::*;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

type Chain = Arc<Mutex<Vec<(String, Value)>>>;

fn rig() -> (tempfile::TempDir, Arc<StoreService>, Chain, Arc<ExposureRegistry>) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let seen: Chain = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let f: clawft_kernel::exposure::AuditFn = Arc::new(move |k, v| s2.lock().unwrap().push((k.to_owned(), v)));
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg.clone())),
        Arc::new(ChainAudit(Some(f))),
    )
    .unwrap();
    (d, Arc::new(svc), seen, reg)
}

/// Install a map with `WRITER_UID` as `agent:writer`; the guard keeps other map tests out.
fn mapped() -> std::sync::MutexGuard<'static, ()> {
    let g = TEST_MAP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let own = nix::unistd::geteuid().as_raw();
    install(Some(PrincipalMap::parse(&format!("[uids]\n{WRITER_UID} = \"agent:writer\"\n"), own).unwrap()));
    g
}

/// What the daemon does after the gate: `principal_route`, then `run_routed` for the principal's
/// verified name, otherwise the operator path (`store_worktree::handle`, no actor).
fn go(svc: &StoreService, who: &CallerCtx, verb: &str, params: Value, now: u64) -> Result<Value, String> {
    let method = format!("store.worktree.{verb}");
    match principal_route(who, &method) {
        Some(actor) => run_routed(svc, "local", &method, params, now, &actor),
        None => crate::store_worktree::run_as(svc, "local", &method, params, now, None),
    }
}

fn writer() -> CallerCtx {
    caller(WRITER_UID)
}

fn operator() -> CallerCtx {
    caller(nix::unistd::geteuid().as_raw())
}

fn new_wt(slug: &str, branch: &str) -> Value {
    json!({"project": PROJ, "slug": slug, "branch": branch, "base": "1", "agent": "evil-label", "host": "evil-host"})
}

/// A worktree with a published tree, made as the operator.
fn published(svc: &StoreService, slug: &str) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("docs/plan.md"), "# Plan\n").unwrap();
    let z = dir.path().join("paths.z");
    std::fs::write(&z, b"docs/plan.md\0").unwrap();
    let c = dir.path().join("chg.z");
    std::fs::write(&c, b"A\0docs/plan.md\0").unwrap();
    let (z, c) = (z.to_string_lossy().into_owned(), c.to_string_lossy().into_owned());
    let op = operator();
    go(svc, &op, "create", new_wt(slug, slug), 10).unwrap();
    let publish = json!({"project": PROJ, "slug": slug, "root": dir.path().to_string_lossy(), "branch": slug, "base": "1", "head": "h1", "paths_z": z, "uncommitted_z": z, "changes_z": c});
    go(svc, &op, "publish", publish, 20).unwrap();
}

/// L-a, handler side: a principal's `create` that reaches the handler through the daemon re-checks
/// that the slug is not somebody else's live worktree. The gate passed it (the slug looked fresh);
/// a worktree store with grants for another principal and no meta yet is live by now.
#[test]
fn a_principals_create_through_the_daemon_route_rechecks_the_slug() {
    let _g = mapped();
    let (_d, svc, _seen, _reg) = rig();
    let pr = StoreName::project("local", PROJ).unwrap();
    svc.create_store(&pr, "t", Default::default()).unwrap();
    let wt = StoreName::worktree("local", PROJ, "feat-b").unwrap();
    svc.create_store(&wt, "t", Default::default()).unwrap();
    svc.grant(&wt, p("agent:b"), Access::Admin, "t").unwrap();
    let e = go(&svc, &writer(), "create", new_wt("feat-b", "evil"), 10).expect_err("agent:writer holds nothing on feat-b");
    assert!(e.contains("does not hold"), "{e}");
    assert!(weftos_store::promote::read_meta(&crate::store_worktree::Op(svc.operator_objects()), &wt.bucket()).unwrap().is_none(), "nothing was written");
    // The operator is never refused by the re-check.
    go(&svc, &operator(), "create", new_wt("feat-b", "ops"), 20).expect("the operator");
    install(None);
}

/// L-a, handler side: the principal's `create` takes `CREATE_LOCK` for the live check and the write,
/// the operator's does not. Held by the test, the lock stalls the principal and not the operator.
#[test]
fn a_principals_create_through_the_daemon_route_takes_the_create_lock() {
    let _g = mapped();
    let (_d, svc, _seen, _reg) = rig();
    let run = |who: CallerCtx, slug: &'static str| {
        let (svc, (tx, rx)) = (svc.clone(), mpsc::channel());
        std::thread::spawn(move || tx.send(go(&svc, &who, "create", new_wt(slug, slug), 10).is_ok()).unwrap());
        rx
    };
    let lock = crate::store_worktree::CREATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let op = run(operator(), "feat-op");
    assert_eq!(op.recv_timeout(Duration::from_secs(60)), Ok(true), "the operator's create must not wait on the lock");
    let pw = run(writer(), "feat-pw");
    assert!(pw.recv_timeout(Duration::from_millis(500)).is_err(), "the principal's create ran while the lock was held");
    drop(lock);
    assert_eq!(pw.recv_timeout(Duration::from_secs(60)), Ok(true), "the principal's create runs once the lock is free");
    install(None);
}

/// N-r2-1: create, claim, orphan and promote record the verified principal, not `cli`, `sweep@daemon`
/// or the `agent@host` label the caller typed. Everything the verbs write to the activity log and
/// the chain is checked, and the operator's own records are not.
#[test]
fn a_principals_worktree_verbs_record_the_principal() {
    let _g = mapped();
    let (_d, svc, seen, reg) = rig();
    published(&svc, "feat-landed");
    published(&svc, "feat-gone");
    let (chain0, act0) = (seen.lock().unwrap().len(), reg.activity().query(None, 500).iter().map(|r| r.seq).max());
    let w = writer();
    let lbl = json!({"project": PROJ, "agent": "evil-label", "host": "evil-host"});
    let with = |slug: &str, extra: Value| {
        let mut v = lbl.clone();
        v["slug"] = json!(slug);
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    go(&svc, &w, "create", new_wt("feat-new", "n"), 30).unwrap();
    go(&svc, &w, "claim", with("feat-landed", json!({})), 40).unwrap();
    go(&svc, &w, "promote", with("feat-landed", json!({})), 50).unwrap();
    let since = 5 * 24 * 3_600_000;
    let o = go(&svc, &w, "orphan", with("feat-gone", json!({"since": since})), since + 25 * 3_600_000).unwrap();
    assert_eq!(o["new"], json!(true), "{o}");
    let chain: Vec<(String, Value)> = seen.lock().unwrap().split_off(chain0);
    let act: Vec<String> = reg.activity().query(None, 500).into_iter().filter(|r| Some(r.seq) > act0).map(|r| r.caller_node).collect();
    assert!(!chain.is_empty() && !act.is_empty(), "the verbs left no records: {chain:?}");
    for (k, v) in &chain {
        let s = v.to_string();
        assert!(!s.contains("evil-"), "{k} carries the caller's label: {s}");
        assert!(!s.contains("\"cli\"") && !s.contains("sweep@daemon"), "{k} names the operator or the sweep: {s}");
    }
    let named: Vec<&str> = chain.iter().filter_map(|(_, v)| v["actor"].as_str()).collect();
    assert!(named.len() >= 4 && named.iter().all(|a| *a == "agent:writer"), "{named:?}");
    assert!(act.iter().all(|a| a == "agent:writer"), "{act:?}");
    install(None);
}
