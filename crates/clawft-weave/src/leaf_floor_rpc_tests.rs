//! Card e821c460: `leaf.reset_floor` refuses a non-admin and a connected
//! leaf, and recovers a disconnected one with a chain record and with replay
//! protection intact.

use std::sync::Arc;

use clawft_kernel::boot::Kernel;
use clawft_kernel::mesh_leaf::LeafIngress;
use clawft_kernel::mesh_runtime::MeshRuntime;
use clawft_platform::NativePlatform;
use ed25519_dalek::SigningKey;
use serde_json::{json, Value};
use tokio::sync::RwLock;
use weftos_leaf_types::link::{LeafCertificate, SignedPublish};

use crate::capability::CallerCapabilities;
use crate::rpc_ext::{authorize, dispatch_ext, CallerCtx, KernelRef};

struct Rig {
    _dir: tempfile::TempDir,
    ckpt_dir: std::path::PathBuf,
    kernel: KernelRef,
    rt: Arc<MeshRuntime>,
    ingress: Arc<LeafIngress>,
    parent: SigningKey,
    machine_pub: [u8; 32],
    scope: String,
    leaf: SigningKey,
    cert: LeafCertificate,
    id: String,
}

fn issue(rig_parent: &SigningKey, scope: &str, machine_pub: [u8; 32], leaf: &SigningKey, serial: u64) -> LeafCertificate {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    LeafCertificate::issue(rig_parent, scope.into(), machine_pub, leaf.verifying_key().to_bytes(), serial, now - 1, now + 3600, vec!["input.publish".into()]).unwrap()
}

async fn rig(tag: u8, with_ingress: bool) -> Rig { rig_with(tag, with_ingress, true).await }

async fn rig_with(tag: u8, with_ingress: bool, chain_enabled: bool) -> Rig {
    use clawft_types::config::{ChainConfig, Config, KernelConfig};
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap(); }
    let ckpt_dir = tempfile::tempdir().unwrap().keep();
    let mut chain = ChainConfig::isolated_in(&ckpt_dir);
    chain.enabled = chain_enabled;
    let kcfg = KernelConfig { chain: Some(chain), ..KernelConfig::default() };
    let k = Kernel::boot(Config::default(), kcfg, Arc::new(NativePlatform::new())).await.expect("kernel boots");
    let router = k.a2a_router().clone();
    let kernel: KernelRef = Arc::new(RwLock::new(k));

    let parent = SigningKey::from_bytes(&[tag; 32]);
    let machine = SigningKey::from_bytes(&[tag + 1; 32]);
    let machine_pub = machine.verifying_key().to_bytes();
    let leaf = SigningKey::from_bytes(&[tag + 2; 32]);
    let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
    let scope = format!("user:{user_id}");
    let cert = issue(&parent, &scope, machine_pub, &leaf, 1);
    let id = cert.leaf_id();
    let ingress = Arc::new(LeafIngress::open(dir.path(), machine).unwrap());

    let mut rt = MeshRuntime::new("srv".into());
    rt.set_local_router(router.clone());
    let rt = Arc::new(rt);
    router.set_mesh_runtime(rt.clone());
    if with_ingress {
        rt.set_leaf_ingress(ingress.clone()).ok().unwrap();
    }
    let r = Rig { _dir: dir, ckpt_dir, kernel, rt, ingress, parent, machine_pub, scope, leaf, cert, id };
    r.enrol();
    r
}

impl Rig {
    fn enrol(&self) {
        std::fs::write(self._dir.path().join("registry").join(format!("{}.json", self.id)), json!({"node_id": self.id, "cert": self.cert}).to_string()).unwrap();
    }
    /// The owner re-issues the certificate (same key, new serial) and re-enrols it.
    fn reissue(&mut self, serial: u64) {
        self.cert = issue(&self.parent, &self.scope, self.machine_pub, &self.leaf, serial);
        self.enrol();
    }
    fn frame(&self, seq: u64, body: &[u8]) -> SignedPublish {
        SignedPublish::sign(self.cert.clone(), &self.leaf, seq, format!("mesh.leaf.{}.input", self.id), body.to_vec()).unwrap()
    }
    fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() }
    fn accept(&self, seq: u64, body: &[u8]) {
        let f = self.frame(seq, body);
        self.ingress.prepare(&f, Self::now()).expect("accepted");
        self.ingress.commit(&f).unwrap();
    }
    /// The full request path: capability check, then the handler.
    async fn call(&self, scope: &str, params: Value) -> Result<Value, (Option<String>, String)> {
        let mut params = params;
        if let Some(o) = params.as_object_mut() { o.entry("confirm").or_insert(json!(true)); }
        let caller = CallerCtx::default().with_peer_key(Some("uid:501".into()));
        let caps = CallerCapabilities::from_scopes([scope]);
        if let Err(r) = authorize(&caller, &caps, "leaf.reset_floor", &params, &self.kernel).await {
            return Err((r.error_kind.clone(), r.error.unwrap_or_default()));
        }
        let r = dispatch_ext(&caller, &caps, "leaf.reset_floor", &params, &self.kernel).await.expect("routed");
        if r.ok { Ok(r.result.unwrap()) } else { Err((r.error_kind.clone(), r.error.unwrap_or_default())) }
    }
    async fn chain_events(&self, kind: &str) -> Vec<Value> {
        let k = self.kernel.read().await;
        k.chain_manager().unwrap().tail(0).into_iter()
            .filter(|e| e.source == "leaf" && e.kind == kind)
            .filter_map(|e| e.payload).collect()
    }
}

impl Rig {
    /// The events on disk, read the way a restart would.
    fn disk_events(&self, kind: &str) -> usize {
        let rvf = self.ckpt_dir.join(clawft_types::config::chain_paths::CHAIN_CHECKPOINT_FILE).with_extension("rvf");
        let Ok(cm) = clawft_kernel::chain::ChainManager::load_from_rvf(&rvf, 1000) else { return 0 };
        cm.tail(0).into_iter().filter(|e| e.source == "leaf" && e.kind == kind).count()
    }
    /// Serve 1..=44 before the reflash.
    fn fill(&self) { for seq in 1..=44 { self.accept(seq, b"x"); } }
}

fn kind(e: &(Option<String>, String)) -> &str { e.0.as_deref().unwrap_or("") }

#[tokio::test]
async fn a_non_admin_caller_is_refused_and_the_floor_stays() {
    let mut r = rig(90, true).await;
    r.fill();
    r.reissue(2);
    for scope in ["read", "write"] {
        let err = r.call(scope, json!({"leaf_id": r.id})).await.unwrap_err();
        assert!(err.1.contains("permission denied"), "{scope}: {err:?}");
    }
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
    assert!(r.chain_events("leaf.floor.reset").await.is_empty());
}

#[tokio::test]
async fn a_reset_must_say_the_pad_was_reflashed() {
    let mut r = rig(95, true).await;
    r.fill();
    r.reissue(2);
    for params in [json!({"leaf_id": r.id, "confirm": false}), json!({"leaf_id": r.id, "confirm": null}), json!({"leaf_id": r.id, "confirm": "yes"})] {
        // `call` fills only a missing confirm; these carry their own.
        let err = r.call("admin", params.clone()).await.unwrap_err();
        assert!(kind(&err) == "leaf_not_confirmed" || err.1.contains("invalid params"), "{params}: {err:?}");
    }
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
    assert!(r.chain_events("leaf.floor.reset").await.is_empty());
}

#[tokio::test]
async fn a_connected_leaf_is_refused_and_the_floor_stays() {
    let mut r = rig(100, true).await;
    r.fill();
    r.reissue(2);
    // A route registered under the leaf id (a live certified connection).
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    r.rt.add_peer(r.id.clone(), tx);
    let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_connected"), "{err:?}");
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
    assert!(r.chain_events("leaf.floor.reset").await.is_empty(), "a refusal is not recorded as a reset");
    r.rt.disconnect_peer(&r.id);

    // A subscription to its push topic also counts as connected.
    r.rt.register_peer_topic(&weftos_leaf_types::push_topic(&r.id), &r.id);
    let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_connected"), "{err:?}");
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
}

#[tokio::test]
async fn a_leaf_that_has_connected_but_not_yet_spoken_blocks_the_reset() {
    let mut r = rig(105, true).await;
    r.fill();
    r.reissue(2);
    // A connection on the leaf listener whose first frame has not arrived
    // names nobody yet, so it cannot be matched to this leaf.
    let pending = r.rt.leaf_pending_guard();
    let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_connecting"), "{err:?}");
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
    assert!(r.chain_events("leaf.floor.reset").await.is_empty());
    drop(pending);
    assert_eq!(r.rt.leaf_pending(), 0);
    r.call("admin", json!({"leaf_id": r.id})).await.expect("reset once the connection is gone");
}

#[tokio::test]
async fn a_reset_without_a_reissued_certificate_is_refused_and_unrecorded() {
    let mut r = rig(108, true).await;
    r.fill();
    let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_cert_unchanged"), "{err:?}");
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
    assert!(r.chain_events("leaf.floor.reset").await.is_empty());
    assert!(r.chain_events("leaf.floor.reset_failed").await.is_empty());
    // A floor written before the binding cannot be proven either.
    let (seq, digest) = r.ingress.floor_of(&r.id).unwrap();
    let floor = r._dir.path().join("floors").join(format!("{}.json", r.id));
    std::fs::write(&floor, json!({"seq": seq, "digest": digest}).to_string()).unwrap();
    r.reissue(2);
    let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_floor_unbound"), "{err:?}");
    assert!(floor.exists());
    assert!(r.chain_events("leaf.floor.reset").await.is_empty());
}

#[tokio::test]
async fn a_disconnected_leaf_is_reset_recorded_and_stays_replay_protected() {
    let mut r = rig(110, true).await;
    for seq in 1..=44 { r.accept(seq, b"old"); }
    let old_digest = r.ingress.floor_of(&r.id).unwrap().1;
    // What a LAN observer recorded before the reset.
    let captured: Vec<SignedPublish> = (1..=3).map(|s| r.frame(s, b"old")).collect();
    assert!(r.ingress.prepare(&r.frame(1, b"new1"), Rig::now()).is_err(), "the reported refusal");
    r.reissue(2);

    let v = r.call("admin", json!({"leaf_id": r.id})).await.expect("reset");
    assert_eq!(v["reset"], true);
    assert_eq!(v["old_seq"], 44);
    assert_eq!(r.ingress.floor_of(&r.id), None);

    // Chain record: who, which leaf, the old floor, when.
    let ev = r.chain_events("leaf.floor.reset").await;
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0]["leaf_id"], r.id.as_str());
    assert_eq!(ev[0]["by"], "uid:501");
    assert_eq!(ev[0]["authenticated"], false, "this caller carries no bearer token");
    assert!(ev[0].get("connection").is_some() && ev[0].get("project").is_some());
    assert_eq!(ev[0]["old_seq"], 44);
    let hex: String = old_digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(ev[0]["old_digest"], hex.as_str());
    assert!(chrono::DateTime::parse_from_rfc3339(ev[0]["at"].as_str().unwrap()).is_ok());
    // M1: the record is on disk, not only in memory.
    assert_eq!(r.disk_events("leaf.floor.reset"), 1);

    // H1: a replay of a captured pre-reset frame is refused.
    for f in &captured {
        assert!(r.ingress.prepare(f, Rig::now()).is_err(), "old frame {} replayed", f.seq);
    }
    // Recovery: the reflashed leaf's sequence from 1 under the new cert is accepted.
    r.accept(1, b"new1");
    r.accept(2, b"new2");
    // Replay protection still holds against the new floor.
    use clawft_kernel::mesh_leaf::Prepare;
    assert_eq!(r.ingress.prepare(&r.frame(2, b"new2"), Rig::now()).unwrap(), Prepare::Duplicate);
    assert!(r.ingress.prepare(&r.frame(1, b"new1"), Rig::now()).is_err(), "a regression is refused");
    assert!(r.ingress.prepare(&r.frame(2, b"other"), Rig::now()).is_err(), "equivocation is refused");
    assert!(r.ingress.prepare(&r.frame(9, b"gap"), Rig::now()).is_err(), "a gap is refused");
}

#[tokio::test]
async fn a_reset_with_no_durable_chain_record_removes_nothing() {
    let mut r = rig(115, true).await;
    r.fill();
    r.reissue(2);
    // The checkpoint directory becomes unusable: a file stands where it was.
    std::fs::remove_dir_all(&r.ckpt_dir).unwrap();
    std::fs::write(&r.ckpt_dir, b"not a directory").unwrap();
    let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_chain_not_durable"), "{err:?}");
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44), "the floor was not removed");
    assert_eq!(r.chain_events("leaf.floor.reset_failed").await.len(), 1);
}

#[tokio::test]
async fn a_failed_removal_is_recorded_after_the_reset_record_is_on_disk() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut r = rig(118, true).await;
        r.fill();
        r.reissue(2);
        let floors = r._dir.path().join("floors");
        std::fs::set_permissions(&floors, std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::File::create(floors.join("probe")).is_ok() {
            // Running as root: the directory mode is not enforced, so the
            // removal cannot be made to fail this way.
            std::fs::remove_file(floors.join("probe")).unwrap();
            std::fs::set_permissions(&floors, std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
        std::fs::set_permissions(&floors, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(err.0.as_deref(), Some("leaf_reset_failed"), "{err:?}");
        assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44), "the floor is still there");
        // The reset record was made durable before the removal was tried.
        assert_eq!(r.disk_events("leaf.floor.reset"), 1);
        assert_eq!(r.chain_events("leaf.floor.reset").await.len(), 1);
        assert_eq!(r.chain_events("leaf.floor.reset_failed").await.len(), 1);
    }
}

#[tokio::test]
async fn a_daemon_without_a_chain_refuses_an_unrecordable_reset() {
    let mut r = rig_with(125, true, false).await;
    r.fill();
    r.reissue(2);
    let err = r.call("admin", json!({"leaf_id": r.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_no_chain"), "{err:?}");
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
}

#[tokio::test]
async fn a_bad_leaf_id_or_a_missing_listener_is_refused() {
    let r = rig(120, true).await;
    r.accept(1, b"x");
    for bad in ["", "..", "../registry/x", "a/b", "ABCDEFABCDEFABCDEFABCDEFABCDEFAB", "abc"] {
        let err = r.call("admin", json!({"leaf_id": bad})).await.unwrap_err();
        assert_eq!(err.0.as_deref(), Some("leaf_bad_id"), "{bad:?}: {err:?}");
    }
    assert!(r.call("admin", json!({})).await.is_err());
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(1));

    let none = rig(130, false).await;
    let err = none.call("admin", json!({"leaf_id": none.id})).await.unwrap_err();
    assert_eq!(err.0.as_deref(), Some("leaf_no_ingress"), "{err:?}");
}
