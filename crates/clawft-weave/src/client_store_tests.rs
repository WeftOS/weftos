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
            log.push(LoggedEvent { seq: 0, source: source.into(), kind: "store.client.gate.grant".into(), payload: Some(p) })
        };
        push("store.client", grant_ev("rival", &me));
        push("store.client", grant_ev("acme", &Authority([7; 32]).to_string()));
        push("kernel", grant_ev("acme", &me));
        log.push(LoggedEvent { seq: 0, source: "store.client".into(), kind: "store.client.gate.grant".into(), payload: None });
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
            seq: 0,
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

    // ---- compaction (S3's design applied to the gate) ----

    use crate::client_store_snapshot::{Ed25519Key, GateSnapshot, SNAPSHOT_KIND, SignedGateSnapshot, SnapshotKey, signing_bytes, state_hash};
    use crate::client_store_state::SOURCE;

    fn sk() -> Arc<dyn SnapshotKey> {
        Arc::new(Ed25519Key(SigningKey::from_bytes(&[5; 32])))
    }
    fn open_k(root: &Path, log: &Arc<MemLog>, clock: fn() -> u64, key: Option<Arc<dyn SnapshotKey>>) -> Result<ClientStore, StoreError> {
        let be = Arc::new(FsBackend::open(&root.join("data")).unwrap());
        let mut j = JournalState::new(log.clone(), mesh(), "acme");
        if let Some(k) = key {
            j = j.with_key(k);
        }
        ClientStore::open_with(spec("acme", root), mesh(), be.clone(), be, Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(NoChain), Some(Arc::new(j)), clock)
    }
    fn xname() -> String {
        format!("weftos://{}/projects/{ULID}/store", mesh())
    }
    fn grant_for(seq: u64, ttl: u64) -> CrossMeshGrant {
        CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &xname(), Access::Read, seq, MS / 1000 - 10, MS / 1000 + ttl)
    }
    /// Some changes of every kind.
    fn churn(s: &ClientStore) -> StoreName {
        let name = s.hold_project(ULID, "op").unwrap();
        s.grant_local(&cog("c1"), &name, Access::Write, "op").unwrap();
        s.grant_local(&cog("c2"), &name, Access::Write, "op").unwrap();
        s.revoke_local(&cog("c2"), &name, "op").unwrap();
        s.grant_weftos("weaver", &name, Access::Read, "owner").unwrap();
        s.trust_mesh([1; 32], key().verifying_key()).unwrap();
        s.install_xmesh(grant_for(1, 100), "owner").unwrap();
        name
    }
    fn state(s: &ClientStore) -> String {
        crate::client_store_snapshot::canonical(&s.gate().state_json(MS / 1000))
    }
    fn without_snapshots(log: &MemLog) -> Arc<MemLog> {
        let out = Arc::new(MemLog::default());
        for e in log.all().into_iter().filter(|e| e.kind != SNAPSHOT_KIND) {
            out.push(e);
        }
        out
    }
    fn snapshots(log: &MemLog) -> Vec<SignedGateSnapshot> {
        log.all()
            .into_iter()
            .filter(|e| e.kind == SNAPSHOT_KIND)
            .map(|e| serde_json::from_value(e.payload.unwrap()["signed"].clone()).unwrap())
            .collect()
    }
    /// Append `snap` as a snapshot record, signed by `signer`, with its state hash recomputed if asked.
    fn publish(log: &MemLog, mut snap: GateSnapshot, rehash: bool, signer: &Ed25519Key) {
        if rehash {
            snap.state_hash = state_hash(&snap.state);
        }
        let (id, sig) = signer.sign(&signing_bytes(&snap)).unwrap();
        let signed = SignedGateSnapshot { snapshot: snap, signer: id, signature: hex::encode(sig) };
        log.append(SOURCE, SNAPSHOT_KIND, json!({"account": "acme", "mesh": mesh().to_string(), "signed": signed}));
    }
    fn refused(r: Result<Vec<u8>, StoreError>) -> bool {
        matches!(r, Err(StoreError::Refused(_)))
    }

    #[test]
    fn snapshot_plus_tail_gives_the_same_gate_as_a_full_replay() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let name = {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            let name = churn(&s);
            assert_eq!(s.events_since_snapshot(), 7);
            assert!(s.snapshot().unwrap().is_some());
            assert_eq!(s.events_since_snapshot(), 0);
            s.grant_local(&cog("c3"), &name, Access::Read, "op").unwrap();
            s.revoke_weftos("weaver", &name, "owner").unwrap();
            s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).unwrap();
            name
        };
        assert_eq!(snapshots(&log).len(), 1);
        let fast = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        let full = open_k(d.path(), &without_snapshots(&log), clock, Some(sk())).unwrap();
        assert_eq!(state(&fast), state(&full));
        assert_eq!(fast.events_since_snapshot(), 2, "only the tail was replayed");
        assert_eq!(full.events_since_snapshot(), 9);
        let f = ClientCaller::Foreign { mesh: [2; 32], id: "x".into() };
        assert_eq!(fast.get(&f, &name, "k").unwrap(), b"v");
        assert!(refused(fast.get(&ClientCaller::Weftos { id: "weaver".into() }, &name, "k")), "revoked after the snapshot");
        assert!(refused(fast.get(&ClientCaller::Local(cog("c2")), &name, "k")), "revoked before it");
    }

    #[test]
    fn a_pruned_history_restores_from_the_snapshot_and_keeps_the_replay_guard() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let name = {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            let name = churn(&s);
            s.snapshot().unwrap();
            name
        };
        // The segment before the snapshot is archived and pruned.
        log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND));
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        assert!(refused(s.get(&ClientCaller::Local(cog("nobody")), &name, "k")));
        assert!(s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).is_ok());
        assert!(s.get(&ClientCaller::Weftos { id: "weaver".into() }, &name, "k").is_ok());
        assert!(refused(s.get(&ClientCaller::Local(cog("c2")), &name, "k")));
        assert!(s.install_xmesh(grant_for(1, 100), "owner").is_err(), "the seq guard came through the snapshot");
    }

    #[test]
    fn a_snapshot_with_the_state_edited_or_the_wrong_scope_stops_the_store_opening() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            churn(&s);
            s.snapshot().unwrap();
        }
        let good = snapshots(&log).pop().unwrap().snapshot;
        let mut edited = good.clone();
        edited.state["grants"] = json!([]);
        let signer = Ed25519Key(SigningKey::from_bytes(&[5; 32]));
        // Edited without re-hashing: the state does not match its hash.
        let l1 = without_snapshots(&log);
        let (id, sig) = signer.sign(&signing_bytes(&edited)).unwrap();
        l1.append(SOURCE, SNAPSHOT_KIND, json!({"account": "acme", "mesh": mesh().to_string(), "signed": SignedGateSnapshot { snapshot: edited.clone(), signer: id, signature: hex::encode(sig) }}));
        let e = open_k(d.path(), &l1, clock, Some(sk())).err().unwrap().to_string();
        assert!(e.contains("does not match its hash"), "{e}");
        // Another scope under this account's name.
        let l2 = without_snapshots(&log);
        let mut wrong = good.clone();
        wrong.scope = "gate/rival/00".into();
        publish(&l2, wrong, false, &signer);
        let e = open_k(d.path(), &l2, clock, Some(sk())).err().unwrap().to_string();
        assert!(e.contains("expected"), "{e}");
    }

    #[test]
    fn a_validly_signed_false_snapshot_is_caught_when_the_history_is_still_there() {
        let signer = Ed25519Key(SigningKey::from_bytes(&[5; 32]));
        for (lie, want) in [("state", "(state)"), ("head", "(events_head)"), ("count", "more history")] {
            let dir = tempfile::tempdir().unwrap();
            let log = Arc::new(MemLog::default());
            {
                let s = open_k(dir.path(), &log, clock, Some(sk())).unwrap();
                churn(&s);
                s.snapshot().unwrap();
            }
            let mut snap = snapshots(&log).pop().unwrap().snapshot;
            match lie {
                // The same history, but the snapshot also grants the WeftOS account admin.
                "state" => {
                    snap.state["grants"].as_array_mut().unwrap().push(json!({
                        "principal": {"kind": "weftos", "account": null, "mesh": null, "id": "evil"},
                        "store": xname(), "access": "admin"}));
                }
                "head" => snap.events_head = "00".repeat(32),
                _ => snap.event_count -= 1,
            }
            publish(&log, snap, true, &signer);
            let e = open_k(dir.path(), &log, clock, Some(sk())).err().unwrap_or_else(|| panic!("{lie}: a signed lie opened")).to_string();
            assert!(e.contains(want), "{lie}: {e}");
        }
    }

    #[test]
    fn a_snapshot_signed_by_another_key_is_passed_over_for_a_full_replay_when_the_history_is_whole() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let name = {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            let name = churn(&s);
            s.snapshot().unwrap();
            name
        };
        // Reopened with a different chain key (a rotation): the snapshot is skipped, the events remain.
        let other: Arc<dyn SnapshotKey> = Arc::new(Ed25519Key(SigningKey::from_bytes(&[6; 32])));
        let s = open_k(d.path(), &log, clock, Some(other.clone())).unwrap();
        assert_eq!(s.events_since_snapshot(), 7, "replayed from the first event");
        assert!(s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).is_ok());
        // With the history pruned there is nothing safe to rebuild from.
        log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND));
        let e = open_k(d.path(), &log, clock, Some(other)).err().unwrap().to_string();
        assert!(e.contains("no longer here"), "{e}");
    }

    #[test]
    fn an_expired_cross_mesh_grant_is_left_out_of_the_snapshot_and_cannot_be_reinstalled() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            churn(&s);
        }
        // Reopen after the grant ended; replay decides as the live call did, then it is snapshotted.
        {
            let s = open_k(d.path(), &log, far_future, Some(sk())).unwrap();
            s.snapshot().unwrap();
        }
        let snap = snapshots(&log).pop().unwrap().snapshot;
        assert_eq!(snap.state["xmesh"], json!([]), "expired, so not carried");
        assert!(snap.state["seqs"].as_array().is_some_and(|a| !a.is_empty()), "but its seq is");
        log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND));
        let s = open_k(d.path(), &log, far_future, Some(sk())).unwrap();
        assert!(s.install_xmesh(grant_for(1, 100), "owner").is_err());
    }

    #[test]
    fn compaction_is_due_after_enough_changes_and_needs_a_key() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        let name = s.hold_project(ULID, "op").unwrap();
        s.grant_local(&cog("c1"), &name, Access::Read, "op").unwrap();
        assert_eq!(s.compact_if_due(3), None);
        s.grant_local(&cog("c2"), &name, Access::Read, "op").unwrap();
        assert!(s.compact_if_due(3).is_some());
        assert_eq!(s.events_since_snapshot(), 0);
        assert_eq!(s.compact_if_due(3), None);
        // No key: no snapshot, and the daemon's loop just does not compact.
        let d2 = tempfile::tempdir().unwrap();
        let s2 = open_k(d2.path(), &Arc::new(MemLog::default()), clock, None).unwrap();
        s2.hold_project(ULID, "op").unwrap();
        assert!(s2.snapshot().is_err());
        assert_eq!(s2.compact_if_due(1), None);
    }

    #[test]
    fn a_gate_kept_in_a_file_has_nothing_to_compact() {
        let d = tempfile::tempdir().unwrap();
        let s = open("acme", d.path(), Arc::new(MemSink::default()));
        s.hold_project(ULID, "op").unwrap();
        assert_eq!(s.snapshot().unwrap(), None);
        assert_eq!(s.compact_if_due(0), None);
    }

    #[test]
    fn heads_carry_across_restarts_so_a_later_snapshot_still_verifies_against_history() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            churn(&s);
            s.snapshot().unwrap();
        }
        {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            let name = s.hold_project(ULID, "op").unwrap();
            s.grant_local(&cog("c4"), &name, Access::Read, "op").unwrap();
            s.snapshot().unwrap();
        }
        assert_eq!(snapshots(&log).len(), 2);
        // The full history is still here, so the newest snapshot is re-folded and must match.
        let fast = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        let full = open_k(d.path(), &without_snapshots(&log), clock, Some(sk())).unwrap();
        assert_eq!(state(&fast), state(&full));
    }

    /// Accepts a signature from either key; signs with the second.
    struct TwoKeys(Ed25519Key, Ed25519Key);
    impl SnapshotKey for TwoKeys {
        fn sign(&self, m: &[u8]) -> Option<(String, Vec<u8>)> {
            self.1.sign(m)
        }
        fn verify(&self, who: &str, m: &[u8], sig: &[u8], at: u64) -> bool {
            self.0.verify(who, m, sig, at) || self.1.verify(who, m, sig, at)
        }
    }
    fn key_n(n: u8) -> Ed25519Key {
        Ed25519Key(SigningKey::from_bytes(&[n; 32]))
    }

    #[test]
    fn a_junk_snapshot_cannot_make_a_pruned_log_open_on_a_smaller_gate() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            churn(&s);
            s.snapshot().unwrap();
        }
        let mut junk = snapshots(&log).pop().unwrap().snapshot;
        (junk.through_seq, junk.event_count) = (0, 0);
        publish(&log, junk, true, &key_n(7));
        log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND));
        // The honest snapshot cannot be verified under the rotated key and its history is gone; the
        // junk record that claims to cover nothing must not talk the loader into a partial rebuild.
        let e = open_k(d.path(), &log, clock, Some(Arc::new(key_n(6)))).err().unwrap().to_string();
        assert!(e.contains("no longer here"), "{e}");
    }

    #[test]
    fn an_unverifiable_newer_snapshot_over_a_pruned_gap_stops_the_store_opening() {
        for pruned in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let log = Arc::new(MemLog::default());
            let name = {
                let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
                let name = churn(&s);
                s.snapshot().unwrap(); // S1, key 5
                s.grant_local(&cog("c5"), &name, Access::Write, "op").unwrap();
                s.revoke_local(&cog("c1"), &name, "op").unwrap();
                name
            };
            {
                // S2 is signed by key 6 (a rotation); this daemon trusts both.
                let s = open_k(d.path(), &log, clock, Some(Arc::new(TwoKeys(key_n(5), key_n(6))))).unwrap();
                s.snapshot().unwrap();
            }
            if pruned {
                log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND));
            }
            // Now only key 5 is held: S2 fails its signature, S1 verifies.
            let r = open_k(d.path(), &log, clock, Some(sk()));
            if pruned {
                let e = r.err().unwrap().to_string();
                assert!(e.contains("no longer here"), "{e}");
            } else {
                let s = r.unwrap();
                assert!(refused(s.get(&ClientCaller::Local(cog("c1")), &name, "k")), "the revoke after S1 was replayed");
                assert!(!matches!(s.get(&ClientCaller::Local(cog("c5")), &name, "k"), Err(StoreError::Refused(_))));
            }
        }
    }

    #[test]
    fn replaying_an_older_snapshot_does_not_roll_the_gate_back() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let name = {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            let name = churn(&s);
            s.snapshot().unwrap(); // S1: c1 holds write
            s.revoke_local(&cog("c1"), &name, "op").unwrap();
            s.snapshot().unwrap(); // S2: c1 revoked
            name
        };
        let s1 = log.all().into_iter().find(|e| e.kind == SNAPSHOT_KIND).unwrap();
        let s2_through = snapshots(&log).last().unwrap().snapshot.through_seq;
        log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND || e.seq > s2_through));
        log.append(SOURCE, SNAPSHOT_KIND, s1.payload.unwrap()); // S1 copied byte for byte, now last
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        assert!(refused(s.get(&ClientCaller::Local(cog("c1")), &name, "k")), "c1 stays revoked");
    }

    #[test]
    fn snapshots_taken_while_the_gate_is_changing_still_open() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            let name = s.hold_project(ULID, "op").unwrap();
            std::thread::scope(|sc| {
                for t in 0..4 {
                    let (s, name) = (&s, &name);
                    sc.spawn(move || {
                        for i in 0..25 {
                            s.grant_local(&cog(&format!("t{t}-{i}")), name, Access::Read, "op").unwrap();
                        }
                    });
                }
                let s = &s;
                sc.spawn(move || {
                    for _ in 0..30 {
                        s.snapshot().unwrap();
                        std::thread::yield_now();
                    }
                });
            });
        }
        // Every snapshot matches its history, however the threads interleaved.
        let again = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        let full = open_k(d.path(), &without_snapshots(&log), clock, Some(sk())).unwrap();
        assert_eq!(state(&again), state(&full));
    }

    use crate::client_store_snapshot::{RotatingKey, Rotation, key_valid_at};

    #[test]
    fn a_retired_key_stands_only_for_records_before_its_rotation() {
        let (k1, k2, k3) = ([1u8; 32], [2u8; 32], [3u8; 32]);
        let r = [Rotation { old: k1, new: k2, seq: 10 }, Rotation { old: k2, new: k3, seq: 20 }];
        assert!(key_valid_at(&k3, &r, &k3, 9999), "the current key");
        assert!(key_valid_at(&k3, &r, &k2, 19) && !key_valid_at(&k3, &r, &k2, 20));
        assert!(key_valid_at(&k3, &r, &k1, 9) && !key_valid_at(&k3, &r, &k1, 10));
        assert!(!key_valid_at(&k3, &r, &[9u8; 32], 1), "a key never in the chain of rotations");
        // A rotation that does not lead to the current key vouches for nothing.
        assert!(!key_valid_at(&k3, &[Rotation { old: k1, new: k2, seq: 10 }], &k1, 5));
        // A loop in the records does not hang.
        assert!(!key_valid_at(&k3, &[Rotation { old: k3, new: k3, seq: 1 }], &k1, 0));
    }

    #[test]
    fn a_snapshot_signed_before_a_key_rotation_still_verifies_and_one_signed_after_does_not() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let name = {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap(); // key 5
            let name = churn(&s);
            s.snapshot().unwrap();
            name
        };
        let rot_seq = log.append("identity", "identity.key_rotation", json!({}));
        log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND || e.seq >= rot_seq)); // the old segment is pruned
        let rotating: Arc<dyn SnapshotKey> = Arc::new(RotatingKey {
            key: SigningKey::from_bytes(&[8; 32]),
            rotations: vec![Rotation { old: key_n(5).0.verifying_key().to_bytes(), new: SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes(), seq: rot_seq }],
        });
        // The pre-rotation snapshot (key 5) is accepted under the new key's rotation history.
        let s = open_k(d.path(), &log, clock, Some(rotating.clone())).unwrap();
        assert!(s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).is_ok());
        drop(s);
        // Someone holding the retired key forges a snapshot after the rotation.
        let mut forged = snapshots(&log).pop().unwrap().snapshot;
        forged.state["grants"].as_array_mut().unwrap().push(json!({
            "principal": {"kind": "weftos", "account": null, "mesh": null, "id": "evil"},
            "store": xname(), "access": "admin"}));
        publish(&log, forged, true, &key_n(5));
        let s = open_k(d.path(), &log, clock, Some(rotating)).unwrap();
        let evil_in_gate = |s: &ClientStore| s.gate().authorize(&Principal::WeftosAccount { id: "evil".into() }, &xname(), Access::Read, MS / 1000).0.is_ok();
        assert!(!evil_in_gate(&s), "the late forgery was passed over");
        // Contrast: a key check blind to rotation takes that same record and the forgery gets in.
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        assert!(evil_in_gate(&s), "without the sequence check the old key forges into the gate (S1's own grant table still refuses)");
        assert!(refused(s.get(&ClientCaller::Weftos { id: "evil".into() }, &name, "k")), "the second layer holds");
    }

    // ---- archive before prune ----

    fn archive_dir(root: &Path) -> std::path::PathBuf {
        root.join("archive")
    }
    fn archive_name(r: &crate::client_store_archive::ArchiveReport) -> String {
        r.path.file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn archive_then_verify_then_prune_and_the_store_still_opens() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let (rep, name) = {
            let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
            assert!(s.archive_gate().unwrap().is_none(), "no snapshot yet, nothing to archive");
            let name = churn(&s);
            s.snapshot().unwrap();
            let rep = s.archive_gate().unwrap().unwrap();
            assert_eq!((rep.header.event_count, rep.header.scope.starts_with("gate/acme/")), (7, true));
            assert_eq!(s.verify_gate_archive(&archive_name(&rep)).unwrap(), rep.header);
            assert!(s.archive_gate().is_err(), "the same segment is not archived twice");
            (rep, name)
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&rep.path), mode(&archive_dir(d.path()))), (0o600, 0o700));
        }
        // The owner prunes the chain segment the archive now holds.
        log.edit(|v| v.retain(|e| e.kind == SNAPSHOT_KIND));
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        assert!(s.put(&ClientCaller::Local(cog("c1")), &name, "k", b"v", false).is_ok());
        assert_eq!(s.verify_gate_archive(&archive_name(&rep)).unwrap(), rep.header, "still verifies after the prune");
        assert!(s.archive_gate().is_err(), "and a pruned segment cannot be archived over it");
    }

    #[test]
    fn a_tampered_truncated_or_unmatched_archive_does_not_verify() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        churn(&s);
        s.snapshot().unwrap();
        let rep = s.archive_gate().unwrap().unwrap();
        let original = std::fs::read_to_string(&rep.path).unwrap();
        let check = |text: String, want: &str| {
            std::fs::write(&rep.path, text).unwrap();
            let e = s.verify_gate_archive(&archive_name(&rep)).unwrap_err().to_string();
            assert!(e.contains(want), "{want}: {e}");
        };
        let lines: Vec<&str> = original.lines().collect();
        check(lines[..lines.len() - 1].join("\n") + "\n", "count or range");
        // Same events, written non-canonically (a space after a colon).
        check(original.replacen("\"kind\":", "\"kind\": ", 1), "canonical form");
        let mut edited = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let i = edited.iter().position(|l| l.contains("cog:c2")).unwrap();
        edited[i] = edited[i].replace("cog:c2", "cog:c7");
        check(edited.join("\n") + "\n", "events_head");
        let mut swapped = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        swapped.swap(1, 2);
        check(swapped.join("\n") + "\n", "chain order");
        // A header edited to claim another range matches no snapshot on the chain.
        check(original.replacen(&format!("\"through_seq\":{}", rep.header.through_seq), "\"through_seq\":999", 1), "");
        std::fs::write(&rep.path, &original).unwrap();
        assert!(s.verify_gate_archive(&archive_name(&rep)).is_ok(), "restored");
    }

    #[test]
    fn a_partial_segment_is_not_archived() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        churn(&s);
        s.snapshot().unwrap();
        log.edit(|v| {
            let i = v.iter().position(|e| e.kind.ends_with(".grant")).unwrap();
            v.remove(i);
        });
        let e = s.archive_gate().unwrap_err().to_string();
        assert!(e.contains("partial segment"), "{e}");
        assert!(!archive_dir(d.path()).join("anything").exists());
    }

    #[test]
    fn archive_names_that_leave_the_archive_directory_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let log = Arc::new(MemLog::default());
        let s = open_k(d.path(), &log, clock, Some(sk())).unwrap();
        for bad in ["../gate.json", "/etc/passwd", "a/b", ".x", ""] {
            assert!(s.verify_gate_archive(bad).unwrap_err().to_string().contains("plain file name"), "{bad:?}");
        }
    }
}
