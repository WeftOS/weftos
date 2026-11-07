use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::SigningKey;
use weftos_store::audit::MemSink;
use weftos_store::service::OpenGate;

use super::*;

const ULID: &str = "01J8ZQ4W2X9K3M5N7P0R2T4V6Y";
const MS: u64 = 1_800_000_000_000;

fn clock() -> u64 {
    MS
}
fn mesh() -> Authority {
    Authority([1; 32])
}
fn key() -> SigningKey {
    SigningKey::from_bytes(&[1; 32])
}
fn spec(account: &str, root: &Path) -> ClientStoreSpec {
    ClientStoreSpec { account: account.into(), uid: 2102, root: root.to_path_buf(), listen: "127.0.0.1:9100".parse().unwrap() }
}
fn open(account: &str, root: &Path, sink: Arc<MemSink>) -> ClientStore {
    let be = Arc::new(FsBackend::open(&root.join("data")).unwrap());
    ClientStore::open(spec(account, root), mesh(), be.clone(), be, Arc::new(OpenGate), sink, clock).unwrap()
}
fn cog(id: &str) -> S1Principal {
    S1Principal::parse(&format!("cog:{id}")).unwrap()
}
fn files(dir: &Path) -> Vec<String> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        v.push(p.display().to_string());
        if p.is_dir() {
            v.extend(files(&p));
        }
    }
    v
}

#[test]
fn a_clients_objects_exist_only_under_its_own_root() {
    let (da, dr) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let acme = open("acme", da.path(), Arc::new(MemSink::default()));
    let rival = open("rival", dr.path(), Arc::new(MemSink::default()));
    let name = acme.hold_project(ULID, "op").unwrap();
    acme.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
    let c1 = ClientCaller::Local(cog("c1"));
    acme.put(&c1, &name, "assets/model.bin", b"acme-secret", false).unwrap();
    assert_eq!(acme.get(&c1, &name, "assets/model.bin").unwrap(), b"acme-secret");
    assert!(files(da.path()).iter().any(|f| f.contains(&name.bucket())), "bytes live under the client root");
    assert!(files(dr.path()).iter().all(|f| !f.contains(&name.bucket())), "and nowhere under another account's root");
    // The other account's instance does not know the store, even for a principal with the same name.
    let r = rival.get(&c1, &name, "assets/model.bin");
    assert!(matches!(r, Err(StoreError::Refused(m)) if m == "refused"));
    rival.grant_local(&cog("c1"), &name, Access::Admin, "op").ok();
    assert!(rival.get(&c1, &name, "assets/model.bin").is_err(), "a grant for a store it does not hold is dead");
}

#[test]
fn weftos_account_is_refused_unless_granted_and_revoke_stops_it() {
    let d = tempfile::tempdir().unwrap();
    let sink = Arc::new(MemSink::default());
    let s = open("acme", d.path(), sink.clone());
    let name = s.hold_project(ULID, "op").unwrap();
    s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
    s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).unwrap();
    let w = ClientCaller::Weftos { id: "weaver".into() };
    assert!(s.get(&w, &name, "k").is_err());
    assert!(s.list(&w, &name, "").is_err());
    assert!(s.put(&w, &name, "k2", b"x", false).is_err());
    assert!(sink.events().iter().any(|e| matches!(&e.outcome, Outcome::Refused(r) if r == "gate:weftos_not_granted")));
    s.grant_weftos("weaver", &name, Access::Read, "owner").unwrap();
    assert_eq!(s.get(&w, &name, "k").unwrap(), b"v");
    assert!(s.put(&w, &name, "k2", b"x", false).is_err(), "read does not write");
    // Impersonating the reserved member through the local door does not work.
    let fake = ClientCaller::Local(S1Principal::new(PrincipalKind::Member, "weftos-weaver").unwrap());
    assert!(s.get(&fake, &name, "k").is_err());
    assert!(s.grant_local(&S1Principal::new(PrincipalKind::Member, "weftos-x").unwrap(), &name, Access::Admin, "op").is_err());
    s.revoke_weftos("weaver", &name, "owner").unwrap();
    assert!(s.get(&w, &name, "k").is_err(), "revoke refuses the next call");
}

#[test]
fn every_cause_gives_the_same_refusal() {
    let d = tempfile::tempdir().unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    let name = s.hold_project(ULID, "op").unwrap();
    let other = StoreName::project(&mesh().to_string(), "01J8ZQ4W2X9K3M5N7P0R2T4V6Z").unwrap();
    let msgs: Vec<String> = [
        s.get(&ClientCaller::Weftos { id: "w".into() }, &name, "k"),
        s.get(&ClientCaller::Local(cog("nobody")), &name, "k"),
        s.get(&ClientCaller::Local(cog("nobody")), &other, "k"),
        s.get(&ClientCaller::Foreign { mesh: [2; 32], id: "x".into() }, &name, "k"),
    ]
    .into_iter()
    .map(|r| r.unwrap_err().to_string())
    .collect();
    assert!(msgs.iter().all(|m| *m == msgs[0]), "{msgs:?}");
}

#[test]
fn cross_mesh_principal_needs_the_signed_grant_and_revoke_drops_both_layers() {
    let d = tempfile::tempdir().unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    let name = s.hold_project(ULID, "op").unwrap();
    s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
    s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).unwrap();
    let f = ClientCaller::Foreign { mesh: [2; 32], id: "weaver".into() };
    assert!(s.get(&f, &name, "k").is_err());
    s.trust_mesh([1; 32], key().verifying_key());
    let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &name.to_string(), Access::Read, 1, MS / 1000 - 10, MS / 1000 + 100);
    s.install_xmesh(g.clone(), "owner").unwrap();
    assert_eq!(s.get(&f, &name, "k").unwrap(), b"v");
    assert!(s.put(&f, &name, "k2", b"x", false).is_err());
    let third = ClientCaller::Foreign { mesh: [3; 32], id: "weaver".into() };
    assert!(s.get(&third, &name, "k").is_err());
    s.revoke_xmesh(&g, "owner").unwrap();
    assert!(s.get(&f, &name, "k").is_err(), "revoked on the next call");
    // A forged grant does not install, and leaves nothing in the service.
    let forged = CrossMeshGrant::sign(&SigningKey::from_bytes(&[9; 32]), [1; 32], [2; 32], &name.to_string(), Access::Admin, 2, MS / 1000 - 10, MS / 1000 + 100);
    assert!(s.install_xmesh(forged, "owner").is_err());
    assert!(s.get(&f, &name, "k").is_err());
}

static LATER: AtomicU64 = AtomicU64::new(MS);
fn later() -> u64 {
    LATER.load(Ordering::SeqCst)
}

#[test]
fn expiry_stops_a_cross_mesh_grant_and_sweep_removes_the_backend_grant() {
    let d = tempfile::tempdir().unwrap();
    let be = Arc::new(FsBackend::open(&d.path().join("data")).unwrap());
    let s = ClientStore::open(spec("acme", d.path()), mesh(), be.clone(), be, Arc::new(OpenGate), Arc::new(MemSink::default()), later).unwrap();
    let name = s.hold_project(ULID, "op").unwrap();
    s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
    s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).unwrap();
    s.trust_mesh([1; 32], key().verifying_key());
    let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &name.to_string(), Access::Read, 1, MS / 1000 - 10, MS / 1000 + 100);
    s.install_xmesh(g, "owner").unwrap();
    let f = ClientCaller::Foreign { mesh: [2; 32], id: "w".into() };
    assert!(s.get(&f, &name, "k").is_ok());
    LATER.store(MS + 101_000, Ordering::SeqCst);
    assert!(s.get(&f, &name, "k").is_err(), "expired by time with no sweep");
    assert_eq!(s.sweep("sweeper"), 1);
    assert_eq!(s.sweep("sweeper"), 0);
}

#[test]
fn revoking_a_local_grant_refuses_the_next_call() {
    let d = tempfile::tempdir().unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    let name = s.hold_project(ULID, "op").unwrap();
    s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
    let c1 = ClientCaller::Local(cog("c1"));
    s.put(&c1, &name, "k", b"v", false).unwrap();
    s.revoke_local(&cog("c1"), &name, "op").unwrap();
    assert!(s.get(&c1, &name, "k").is_err());
}

#[test]
fn a_restart_fails_closed_until_the_gate_is_rebuilt() {
    let d = tempfile::tempdir().unwrap();
    let name = {
        let s = open("acme", d.path(), Arc::new(MemSink::default()));
        let name = s.hold_project(ULID, "op").unwrap();
        s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
        s.grant_weftos("weaver", &name, Access::Admin, "owner").unwrap();
        s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).unwrap();
        name
    };
    // The service state persisted; the gate did not. Nobody gets in on the service's grants alone.
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    let (c1, w) = (ClientCaller::Local(cog("c1")), ClientCaller::Weftos { id: "weaver".into() });
    assert!(s.get(&c1, &name, "k").is_err());
    assert!(s.get(&w, &name, "k").is_err());
    s.hold_project(ULID, "op").unwrap();
    assert!(s.get(&c1, &name, "k").is_err(), "holding the project is not a grant");
    assert!(s.get(&w, &name, "k").is_err());
    s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
    assert_eq!(s.get(&c1, &name, "k").unwrap(), b"v", "bytes survived the restart");
    assert!(s.get(&w, &name, "k").is_err(), "the WeftOS account stays out until re-granted");
}

#[cfg(unix)]
#[test]
fn open_fs_needs_a_root_owned_by_the_client_with_mode_0700() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let d = tempfile::tempdir().unwrap();
    let mut sp = spec("acme", d.path());
    sp.uid = std::fs::metadata(d.path()).unwrap().uid();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let open_it = |sp: ClientStoreSpec| ClientStore::open_fs(sp, mesh(), Arc::new(OpenGate), Arc::new(MemSink::default()), clock);
    assert!(open_it(sp.clone()).is_err());
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(open_it(sp).is_ok());
}
