//! `ClientStore` on real RustFS (ADR-126 S5 on S1's transport).
//!
//! Every test here is `#[ignore]`d. They need two THROWAWAY loopback RustFS instances, which
//! `scripts/store/client-live-test.sh` starts (random ports and credentials, temporary data under
//! `target/`) and stops afterwards; it names their env files in
//! `WEFTOS_STORE_TEST_CLIENT_RUSTFS_ENVS` (`a:b`). Never point these at PG or a shared RustFS.
//! The two instances are shared by the tests of one run, so each test uses its own project and
//! its own principals (a principal's RustFS key has one policy, owned by the service that granted it).

use std::path::PathBuf;

use weftos_store::{AdminBackend, BackendError, ObjectBackend, RustfsBackend, RustfsConfig};

// The helpers (`cog`, `files`, `clock`, `mesh`, `key`, `MS`) are the parent test module's.
use super::*;

/// One live RustFS instance, plus a fresh store root as the client user's would be.
struct Instance {
    cfg: RustfsConfig,
    port: u16,
    /// RustFS's own data directory.
    data: PathBuf,
    /// The client store root: owned by this user, mode 0700.
    root: tempfile::TempDir,
}

impl Instance {
    /// Instance `idx` (0 or 1) of the run.
    fn get(idx: usize) -> Self {
        let envs = std::env::var("WEFTOS_STORE_TEST_CLIENT_RUSTFS_ENVS").expect("run via scripts/store/client-live-test.sh");
        let env = PathBuf::from(envs.split(':').nth(idx).expect("two env files"));
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(root.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let cfg = RustfsConfig::from_env_file(&env, root.path().join("secrets")).unwrap();
        std::fs::create_dir_all(&cfg.secrets_dir).unwrap();
        let text = std::fs::read_to_string(&env).unwrap();
        let data = PathBuf::from(text.lines().find_map(|l| l.strip_prefix("DATA_DIR=")).expect("DATA_DIR"));
        let port = cfg.endpoint.rsplit(':').next().unwrap().parse().unwrap();
        Self { cfg, port, data, root }
    }

    /// The `store.env` the client user's daemon reads (mode 0600), inside this store root.
    fn env_file(&self) -> PathBuf {
        let p = self.root.path().join("store.env");
        let c = &self.cfg;
        std::fs::write(&p, format!("MESH_S3_ENDPOINT={}\nMESH_S3_ACCESS_KEY={}\nMESH_S3_SECRET_KEY={}\n", c.endpoint, c.root_access_key, c.root_secret_key)).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        p
    }

    fn spec(&self, account: &str) -> ClientStoreSpec {
        use std::os::unix::fs::MetadataExt;
        ClientStoreSpec {
            account: account.into(),
            uid: std::fs::metadata(self.root.path()).unwrap().uid(),
            root: self.root.path().to_path_buf(),
            listen: format!("127.0.0.1:{}", self.port).parse().unwrap(),
        }
    }

    fn open(&self, account: &str, clock: fn() -> u64) -> ClientStore {
        ClientStore::open_rustfs(self.spec(account), mesh(), &self.env_file(), Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(weftos_store::service::NoChain), None, clock)
            .unwrap()
    }

    fn try_open(&self, account: &str) -> Result<ClientStore, StoreError> {
        ClientStore::open_rustfs(self.spec(account), mesh(), &self.env_file(), Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(weftos_store::service::NoChain), None, clock)
    }

    fn store(&self, account: &str) -> ClientStore {
        self.open(account, clock)
    }

    /// The root-credential backend, for checking what RustFS itself holds.
    fn root_be(&self) -> RustfsBackend {
        RustfsBackend::new(self.cfg.clone()).unwrap()
    }

    /// A backend that speaks as one principal with that principal's own RustFS key (no gate, no
    /// service in the way). The key was created when the principal was granted.
    fn as_key(&self, principal: &str) -> RustfsBackend {
        let id = RustfsBackend::key_id_for(principal);
        let secret = std::fs::read_to_string(self.root_be().secret_path(&id)).unwrap();
        RustfsBackend::new(RustfsConfig { root_access_key: id, root_secret_key: secret, ..self.cfg.clone() }).unwrap()
    }
}

fn forbidden<T: std::fmt::Debug>(r: Result<T, BackendError>) -> bool {
    matches!(r, Err(BackendError::Forbidden(_)))
}

fn project() -> String {
    ulid::Ulid::new().to_string()
}

fn far() -> u64 {
    MS + 1_000_000 * 1000
}

#[test]
#[ignore = "needs two throwaway RustFS: scripts/store/client-live-test.sh"]
fn two_client_accounts_on_two_instances_share_nothing() {
    let (a, r) = (Instance::get(0), Instance::get(1));
    let (acme, rival) = (a.store("acme"), r.store("rival"));
    let name = acme.hold_project(&project(), "op").unwrap();
    acme.grant_local(&cog("iso"), &name, Access::Write, "op").unwrap();
    let c1 = ClientCaller::Local(cog("iso"));
    acme.put(&c1, &name, "assets/model.bin", b"acme-secret", false).unwrap();
    assert_eq!(acme.get(&c1, &name, "assets/model.bin").unwrap(), b"acme-secret");

    // The bucket and its bytes exist on acme's instance and under acme's data directory only.
    let bucket = name.bucket();
    assert!(a.root_be().bucket_exists(&bucket).unwrap());
    assert!(!r.root_be().bucket_exists(&bucket).unwrap(), "rival's RustFS has never heard of it");
    assert!(files(&a.data).iter().any(|f| f.contains(&bucket)));
    assert!(files(&r.data).iter().all(|f| !f.contains(&bucket)));

    // Rival's service refuses the same caller, and acme's keys mean nothing to rival's RustFS.
    assert!(matches!(rival.get(&c1, &name, "assets/model.bin"), Err(StoreError::Refused(m)) if m == "refused"));
    assert_eq!(a.as_key("cog:iso").get(&bucket, "assets/model.bin").unwrap(), b"acme-secret", "acme's own key works on acme's instance");
    let on_rival = {
        let id = RustfsBackend::key_id_for("cog:iso");
        let secret = std::fs::read_to_string(a.root_be().secret_path(&id)).unwrap();
        RustfsBackend::new(RustfsConfig { root_access_key: id, root_secret_key: secret, ..r.cfg.clone() }).unwrap()
    };
    assert!(on_rival.get(&bucket, "assets/model.bin").is_err(), "acme's cog key against rival's instance");
    let acme_root_on_rival =
        RustfsBackend::new(RustfsConfig { root_access_key: a.cfg.root_access_key.clone(), root_secret_key: a.cfg.root_secret_key.clone(), ..r.cfg.clone() }).unwrap();
    assert!(acme_root_on_rival.bucket_exists(&bucket).is_err(), "acme's root credential against rival's instance");
}

#[test]
#[ignore = "needs two throwaway RustFS: scripts/store/client-live-test.sh"]
fn the_weftos_account_is_refused_until_granted_and_revoke_is_refused_at_rustfs_itself() {
    let i = Instance::get(0);
    let s = i.store("acme");
    let name = s.hold_project(&project(), "op").unwrap();
    s.grant_local(&cog("wf"), &name, Access::Write, "op").unwrap();
    s.put(&ClientCaller::Local(cog("wf")), &name, "k", b"v", false).unwrap();
    let (w, bucket) = (ClientCaller::Weftos { id: "weaver".into() }, name.bucket());
    assert!(matches!(s.get(&w, &name, "k"), Err(StoreError::Refused(m)) if m == "refused: refused" || m == "refused"), "refused with no grant");

    s.grant_weftos("weaver", &name, Access::Read, "owner").unwrap();
    assert_eq!(s.get(&w, &name, "k").unwrap(), b"v");
    assert!(s.put(&w, &name, "k2", b"x", false).is_err(), "read does not write");
    // Straight to RustFS with the mapped member's own key: allowed to read, not to write.
    let direct = i.as_key("member:weftos-weaver");
    assert_eq!(direct.get(&bucket, "k").unwrap(), b"v");
    assert!(forbidden(direct.put(&bucket, "k3", b"x", false)));

    s.revoke_weftos("weaver", &name, "owner").unwrap();
    assert!(matches!(s.get(&w, &name, "k"), Err(StoreError::Refused(_))), "the gate refuses the next call");
    assert!(forbidden(direct.get(&bucket, "k")), "and so does RustFS, to the same key, with no gate or service in the way");
}

#[test]
#[ignore = "needs two throwaway RustFS: scripts/store/client-live-test.sh"]
fn a_cross_mesh_grant_reaches_rustfs_and_revoke_and_expiry_drop_it_there() {
    let i = Instance::get(0);
    let s = i.store("acme");
    let name = s.hold_project(&project(), "op").unwrap();
    s.grant_local(&cog("xm"), &name, Access::Write, "op").unwrap();
    s.put(&ClientCaller::Local(cog("xm")), &name, "k", b"v", false).unwrap();
    s.trust_mesh([1; 32], key().verifying_key()).unwrap();
    let f = ClientCaller::Foreign { mesh: [2; 32], id: "w".into() };
    let member = format!("member:xmesh-{}", hex::encode([2u8; 32]));
    let bucket = name.bucket();
    let g = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &name.to_string(), Access::Read, 1, MS / 1000 - 10, MS / 1000 + 100);
    assert!(s.get(&f, &name, "k").is_err());
    s.install_xmesh(g.clone(), "owner").unwrap();
    assert_eq!(s.get(&f, &name, "k").unwrap(), b"v");
    let direct = i.as_key(&member);
    assert_eq!(direct.get(&bucket, "k").unwrap(), b"v", "the grant is a real RustFS policy");
    assert!(forbidden(direct.put(&bucket, "k2", b"x", false)), "read only");

    s.revoke_xmesh(&g, "owner").unwrap();
    assert!(s.get(&f, &name, "k").is_err());
    assert!(forbidden(direct.get(&bucket, "k")), "revoked at RustFS on the next request");

    // A grant that expires: the sweep removes the RustFS policy as well as the gate entry.
    let g2 = CrossMeshGrant::sign(&key(), [1; 32], [2; 32], &name.to_string(), Access::Read, 2, MS / 1000 - 10, MS / 1000 + 100);
    s.install_xmesh(g2, "owner").unwrap();
    assert_eq!(direct.get(&bucket, "k").unwrap(), b"v");
    // The same account's store, reopened after the grant's end (the gate state is the file in its root).
    drop(s);
    let later = i.open("acme", far);
    assert_eq!(later.sweep("sweeper"), 1);
    assert!(forbidden(direct.get(&bucket, "k")), "the expired grant is gone from RustFS");
}

#[test]
#[ignore = "needs two throwaway RustFS: scripts/store/client-live-test.sh"]
fn open_rustfs_refuses_a_shared_endpoint_a_loose_env_file_and_a_foreign_owner() {
    use std::os::unix::fs::PermissionsExt;
    let (mine, other) = (Instance::get(0), Instance::get(1));
    let open = |spec: ClientStoreSpec, env: &Path| {
        ClientStore::open_rustfs(spec, mesh(), env, Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(weftos_store::service::NoChain), None, clock)
    };
    // The env file names another instance than this account's listener.
    let e = open(mine.spec("acme"), &other.env_file()).err().unwrap().to_string();
    assert!(e.contains("not this account's instance"), "{e}");
    // Group or other access to the credential file.
    let env = mine.env_file();
    std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(open(mine.spec("acme"), &env).err().unwrap().to_string().contains("mode 0600"));
    std::fs::set_permissions(&env, std::fs::Permissions::from_mode(0o600)).unwrap();
    // A file for another uid.
    let mut spec = mine.spec("acme");
    spec.uid += 1;
    assert!(open(spec, &env).is_err());
    // A symlinked env file.
    let link = mine.root.path().join("link.env");
    std::os::unix::fs::symlink(&env, &link).unwrap();
    assert!(open(mine.spec("acme"), &link).err().unwrap().to_string().contains("regular file"));
    // And the good case opens.
    assert!(open(mine.spec("acme"), &env).is_ok());
}

#[test]
#[ignore = "needs two throwaway RustFS: scripts/store/client-live-test.sh"]
fn a_second_client_account_on_one_instance_is_refused_and_the_owner_is_not_disturbed() {
    let (i, j) = (Instance::get(0), Instance::get(0));
    let acme = i.store("acme");
    let name = acme.hold_project(&project(), "op").unwrap();
    let e = j.try_open("intruder").err().unwrap().to_string();
    assert!(e.contains("already belongs to client account 'acme'") && e.contains("needs its own RustFS instance"), "{e}");
    assert!(j.root_be().bucket_exists(&name.bucket()).unwrap(), "the owner's data is where it was");
    assert!(j.try_open("acme").is_ok(), "and the owner reopens");
}

/// Why the rule exists: a principal's RustFS key has one policy, owned by whichever service granted
/// it last, so two services on one instance overwrite each other.
#[test]
#[ignore = "needs two throwaway RustFS: scripts/store/client-live-test.sh"]
fn two_services_on_one_instance_overwrite_each_others_key_policy() {
    use weftos_store::service::{Config, NoChain, Parties};
    use weftos_store::{Access as A, Principal as P, StoreService};
    let i = Instance::get(0);
    let svc = || {
        let be = Arc::new(i.root_be());
        StoreService::open(Config { node_id: "x".into(), state_path: None, clock }, be.clone(), be, Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(NoChain)).unwrap()
    };
    let (one, two) = (svc(), svc());
    let (a, b) = (StoreName::project(&mesh().to_string(), &project()).unwrap(), StoreName::project(&mesh().to_string(), &project()).unwrap());
    let who = P::parse("cog:policy-war").unwrap();
    one.create_store(&a, "op", Parties::default()).unwrap();
    two.create_store(&b, "op", Parties::default()).unwrap();
    one.grant(&a, who.clone(), A::Write, "op").unwrap();
    let key = i.as_key("cog:policy-war");
    assert!(key.put(&a.bucket(), "k", b"v", false).is_ok(), "the first service's grant works");
    two.grant(&b, who, A::Write, "op").unwrap();
    assert!(forbidden(key.put(&a.bucket(), "k2", b"v", false)), "the second service's grant replaced the first's at RustFS, though service one still lists it");
}

#[test]
#[ignore = "needs two throwaway RustFS: scripts/store/client-live-test.sh"]
fn the_client_store_rides_the_daemons_own_service_on_the_same_rustfs() {
    use weftos_store::service::{Config, NoChain};
    let i = Instance::get(0);
    let be = Arc::new(i.root_be());
    let daemon_svc = Arc::new(
        weftos_store::StoreService::open(Config { node_id: "client-daemon".into(), state_path: None, clock }, be.clone(), be, Arc::new(OpenGate), Arc::new(MemSink::default()), Arc::new(NoChain)).unwrap(),
    );
    let cs = ClientStore::open_rustfs_shared(i.spec("acme"), mesh(), &i.env_file(), daemon_svc.clone(), Arc::new(MemSink::default()), None, clock).unwrap();
    let name = cs.hold_project(&project(), "op").unwrap();
    cs.grant_local(&cog("sh"), &name, Access::Write, "op").unwrap();
    let c = ClientCaller::Local(cog("sh"));
    cs.put(&c, &name, "k", b"v", false).unwrap();
    assert_eq!(cs.get(&c, &name, "k").unwrap(), b"v");
    assert!(daemon_svc.list_stores().iter().any(|s| s.name == name.to_string()), "the daemon's one service holds the client's store");
    assert_eq!(i.as_key("cog:sh").get(&name.bucket(), "k").unwrap(), b"v");
}
