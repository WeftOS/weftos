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
    s.trust_mesh([1; 32], key().verifying_key()).unwrap();
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
    s.trust_mesh([1; 32], key().verifying_key()).unwrap();
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
fn a_restart_keeps_the_gate_and_a_revoke_stays_revoked() {
    let d = tempfile::tempdir().unwrap();
    let (c1, w) = (ClientCaller::Local(cog("c1")), ClientCaller::Weftos { id: "weaver".into() });
    let f = ClientCaller::Foreign { mesh: [2; 32], id: "x".into() };
    let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &format!("weftos://{}/projects/{ULID}/store", mesh()), Access::Read, 1, MS / 1000 - 10, MS / 1000 + 100);
    let name = {
        let s = open("acme", d.path(), Arc::new(MemSink::default()));
        let name = s.hold_project(ULID, "op").unwrap();
        s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
        s.grant_weftos("weaver", &name, Access::Read, "owner").unwrap();
        s.grant_weftos("gone", &name, Access::Read, "owner").unwrap();
        s.revoke_weftos("gone", &name, "owner").unwrap();
        s.trust_mesh([1; 32], key().verifying_key()).unwrap();
        s.install_xmesh(g.clone(), "owner").unwrap();
        s.put(&c1, &name, "k", b"v", false).unwrap();
        name
    };
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    assert_eq!(s.get(&c1, &name, "k").unwrap(), b"v");
    assert_eq!(s.get(&w, &name, "k").unwrap(), b"v");
    assert_eq!(s.get(&f, &name, "k").unwrap(), b"v", "the cross-mesh grant survived");
    assert!(s.get(&ClientCaller::Weftos { id: "gone".into() }, &name, "k").is_err());
    assert!(s.install_xmesh(g.clone(), "owner").is_err(), "the replay counter survived");
    s.revoke_xmesh(&g, "owner").unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    assert!(s.get(&f, &name, "k").is_err(), "a revoke survives a restart");
    assert!(s.install_xmesh(g, "owner").is_err(), "and the revoked grant cannot come back");
}

#[test]
fn unreadable_gate_state_does_not_open_the_store() {
    let d = tempfile::tempdir().unwrap();
    {
        let s = open("acme", d.path(), Arc::new(MemSink::default()));
        s.hold_project(ULID, "op").unwrap();
    }
    let be = || Arc::new(FsBackend::open(&d.path().join("data")).unwrap());
    let reopen = |account: &str, m: Authority| ClientStore::open(spec(account, d.path()), m, be(), be(), Arc::new(OpenGate), Arc::new(MemSink::default()), clock);
    assert!(reopen("acme", mesh()).is_ok());
    assert!(reopen("rival", mesh()).is_err(), "another account's gate state is not accepted");
    assert!(reopen("acme", Authority([7; 32])).is_err(), "nor another mesh's");
    std::fs::write(d.path().join("gate.json"), b"{ not json").unwrap();
    assert!(reopen("acme", mesh()).is_err(), "a corrupt file fails closed");
}

#[cfg(unix)]
#[test]
fn the_gate_file_is_private_to_the_client() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    s.hold_project(ULID, "op").unwrap();
    let m = std::fs::metadata(d.path().join("gate.json")).unwrap();
    assert_eq!(m.permissions().mode() & 0o777, 0o600);
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

#[test]
fn revoke_local_cannot_reach_the_reserved_members() {
    let d = tempfile::tempdir().unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    let name = s.hold_project(ULID, "op").unwrap();
    s.grant_weftos("weaver", &name, Access::Read, "owner").unwrap();
    let fake = S1Principal::new(PrincipalKind::Member, "weftos-weaver").unwrap();
    assert!(s.revoke_local(&fake, &name, "op").is_err());
}

#[test]
fn reserved_principals_leave_an_audit_line_and_the_same_refusal() {
    let d = tempfile::tempdir().unwrap();
    let sink = Arc::new(MemSink::default());
    let s = open("acme", d.path(), sink.clone());
    let name = s.hold_project(ULID, "op").unwrap();
    let fake = ClientCaller::Local(S1Principal::new(PrincipalKind::Member, "xmesh-abc").unwrap());
    assert!(matches!(s.get(&fake, &name, "k"), Err(StoreError::Refused(m)) if m == "refused"));
    assert!(sink.events().iter().any(|e| matches!(&e.outcome, Outcome::Refused(r) if r == "gate:reserved_or_invalid_principal")));
}

#[test]
fn a_service_refusal_reads_the_same_as_a_gate_refusal() {
    let d = tempfile::tempdir().unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    let name = s.hold_project(ULID, "op").unwrap();
    // The gate has a grant the service does not: only the service refuses.
    s.gate().grant(StoreGrant { principal: Principal::Client { account: "acme".into(), id: cog("c9").to_string() }, store: name.to_string(), access: Access::Read });
    let a = s.get(&ClientCaller::Local(cog("c9")), &name, "k").unwrap_err().to_string();
    let b = s.get(&ClientCaller::Local(cog("nobody")), &name, "k").unwrap_err().to_string();
    assert_eq!(a, b);
}

#[test]
fn revoking_a_replaced_cross_mesh_grant_leaves_the_newer_one_working() {
    let d = tempfile::tempdir().unwrap();
    let s = open("acme", d.path(), Arc::new(MemSink::default()));
    let name = s.hold_project(ULID, "op").unwrap();
    s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
    s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).unwrap();
    s.trust_mesh([1; 32], key().verifying_key()).unwrap();
    let mk = |seq| CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &name.to_string(), Access::Read, seq, MS / 1000 - 10, MS / 1000 + 100);
    let (old, new) = (mk(1), mk(2));
    s.install_xmesh(old.clone(), "owner").unwrap();
    s.install_xmesh(new, "owner").unwrap();
    s.revoke_xmesh(&old, "owner").unwrap();
    let f = ClientCaller::Foreign { mesh: [2; 32], id: "w".into() };
    assert_eq!(s.get(&f, &name, "k").unwrap(), b"v", "the newer grant still stands");
}

#[cfg(unix)]
#[test]
fn open_fs_refuses_a_symlinked_data_dir() {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    let d = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let mut sp = spec("acme", d.path());
    sp.uid = std::fs::metadata(d.path()).unwrap().uid();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(elsewhere.path(), d.path().join("data")).unwrap();
    assert!(ClientStore::open_fs(sp, mesh(), Arc::new(OpenGate), Arc::new(MemSink::default()), clock).is_err());
}

mod journal {
    use serde_json::json;
    use weftos_store::service::NoChain;

    use super::*;
    use crate::client_store_state::{EventLog, JournalState, LoggedEvent, MemLog};

    fn far_future() -> u64 {
        MS + 1_000_000 * 1000
    }
    fn open_j(account: &str, root: &Path, log: &Arc<MemLog>, clock: fn() -> u64, m: Authority) -> Result<ClientStore, StoreError> {
        let be = Arc::new(FsBackend::open(&root.join("data")).unwrap());
        let state = Arc::new(JournalState::new(log.clone(), m, account));
        ClientStore::open_with(spec(account, root), m, be.clone(), be, Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(NoChain), Some(state), clock)
    }

    #[test]
    fn the_gate_is_rebuilt_from_the_journal_with_no_gate_file() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &format!("weftos://{}/projects/{ULID}/store", mesh()), Access::Read, 1, MS / 1000 - 10, MS / 1000 + 100);
        let (c1, w, f) = (ClientCaller::Local(cog("c1")), ClientCaller::Weftos { id: "weaver".into() }, ClientCaller::Foreign { mesh: [2; 32], id: "x".into() });
        let name = {
            let s = open_j("acme", d.path(), &log, clock, mesh()).unwrap();
            let name = s.hold_project(ULID, "op").unwrap();
            s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
            s.grant_weftos("weaver", &name, Access::Read, "owner").unwrap();
            s.grant_weftos("gone", &name, Access::Read, "owner").unwrap();
            s.revoke_weftos("gone", &name, "owner").unwrap();
            s.trust_mesh([1; 32], key().verifying_key()).unwrap();
            s.install_xmesh(g.clone(), "owner").unwrap();
            s.put(&c1, &name, "k", b"v", false).unwrap();
            name
        };
        assert!(!d.path().join("gate.json").exists(), "the journal replaces the file");
        let kinds: Vec<_> = log.all().into_iter().map(|e| e.kind).collect();
        assert!(kinds.iter().all(|k| k.starts_with("store.client.gate.")), "{kinds:?}");
        assert_eq!(kinds.first().unwrap(), "store.client.gate.hold");
        let s = open_j("acme", d.path(), &log, clock, mesh()).unwrap();
        assert_eq!(s.get(&c1, &name, "k").unwrap(), b"v");
        assert_eq!(s.get(&w, &name, "k").unwrap(), b"v");
        assert_eq!(s.get(&f, &name, "k").unwrap(), b"v");
        assert!(matches!(s.get(&ClientCaller::Weftos { id: "gone".into() }, &name, "k"), Err(StoreError::Refused(_))), "a revoke replays after its grant");
        assert!(s.install_xmesh(g, "owner").is_err(), "the replay counter was rebuilt");
    }

    #[test]
    fn replay_drops_an_expired_grant_and_keeps_a_revoke_revoked() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &format!("weftos://{}/projects/{ULID}/store", mesh()), Access::Read, 1, MS / 1000 - 10, MS / 1000 + 100);
        let name = {
            let s = open_j("acme", d.path(), &log, clock, mesh()).unwrap();
            let name = s.hold_project(ULID, "op").unwrap();
            s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
            s.trust_mesh([1; 32], key().verifying_key()).unwrap();
            s.install_xmesh(g, "owner").unwrap();
            s.grant_local(&cog("c2"), &name, Access::Write, "op").unwrap();
            s.revoke_local(&cog("c2"), &name, "op").unwrap();
            name
        };
        let s = open_j("acme", d.path(), &log, far_future, mesh()).unwrap();
        let f = ClientCaller::Foreign { mesh: [2; 32], id: "x".into() };
        let refused = |r: Result<Vec<u8>, StoreError>| matches!(r, Err(StoreError::Refused(_)));
        assert!(refused(s.get(&f, &name, "k")), "expired while the daemon was down");
        assert!(refused(s.get(&ClientCaller::Local(cog("c2")), &name, "k")), "a revoke replays after its grant");
        assert!(s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).is_ok(), "c1's grant has no expiry");
    }

    #[test]
    fn another_accounts_or_meshs_events_and_other_sources_are_ignored() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let name = {
            let s = open_j("acme", d.path(), &log, clock, mesh()).unwrap();
            s.hold_project(ULID, "op").unwrap()
        };
        let grant_ev = |account: &str, mesh_hex: &str| {
            json!({"account": account, "mesh": mesh_hex, "ev": {
                "principal": {"kind": "weftos", "account": null, "mesh": null, "id": "weaver"},
                "store": name.to_string(), "access": "admin"}})
        };
        let me = mesh().to_string();
        let push = |source: &str, p: serde_json::Value| {
            log.push(LoggedEvent { source: source.into(), kind: "store.client.gate.grant".into(), payload: Some(p) })
        };
        push("store.client", grant_ev("rival", &me));
        push("store.client", grant_ev("acme", &Authority([7; 32]).to_string()));
        push("kernel", grant_ev("acme", &me));
        log.push(LoggedEvent { source: "store.client".into(), kind: "store.client.gate.grant".into(), payload: None });
        let s = open_j("acme", d.path(), &log, clock, mesh()).unwrap();
        let w = ClientCaller::Weftos { id: "weaver".into() };
        assert!(matches!(s.get(&w, &name, "k"), Err(StoreError::Refused(_))), "none of those grants reached acme's gate");
    }

    #[test]
    fn a_garbled_event_for_this_account_stops_the_store_opening() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        open_j("acme", d.path(), &log, clock, mesh()).unwrap();
        log.push(LoggedEvent {
            source: "store.client".into(),
            kind: "store.client.gate.grant".into(),
            payload: Some(json!({"account": "acme", "mesh": mesh().to_string(), "ev": {"principal": 5}})),
        });
        assert!(open_j("acme", d.path(), &log, clock, mesh()).is_err());
    }

    #[test]
    fn a_journal_for_another_account_or_mesh_is_not_accepted() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let be = Arc::new(FsBackend::open(&d.path().join("data")).unwrap());
        let state = Arc::new(JournalState::new(log, mesh(), "acme"));
        let r = ClientStore::open_with(spec("rival", d.path()), mesh(), be.clone(), be, Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(NoChain), Some(state), clock);
        assert!(r.is_err());
    }
}
