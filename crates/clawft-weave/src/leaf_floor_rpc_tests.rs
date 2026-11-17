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
    kernel: KernelRef,
    rt: Arc<MeshRuntime>,
    ingress: Arc<LeafIngress>,
    leaf: SigningKey,
    cert: LeafCertificate,
    id: String,
}

async fn rig(tag: u8, with_ingress: bool) -> Rig {
    use clawft_types::config::{ChainConfig, Config, KernelConfig};
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap(); }
    let kcfg = KernelConfig { chain: Some(ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())), ..KernelConfig::default() };
    let k = Kernel::boot(Config::default(), kcfg, Arc::new(NativePlatform::new())).await.expect("kernel boots");
    let router = k.a2a_router().clone();
    let kernel: KernelRef = Arc::new(RwLock::new(k));

    let parent = SigningKey::from_bytes(&[tag; 32]);
    let machine = SigningKey::from_bytes(&[tag + 1; 32]);
    let leaf = SigningKey::from_bytes(&[tag + 2; 32]);
    let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, now - 1, now + 3600, vec!["input.publish".into()]).unwrap();
    let id = cert.leaf_id();
    let ingress = Arc::new(LeafIngress::open(dir.path(), machine).unwrap());
    std::fs::write(dir.path().join("registry").join(format!("{id}.json")), json!({"node_id": id, "cert": cert}).to_string()).unwrap();

    let mut rt = MeshRuntime::new("srv".into());
    rt.set_local_router(router.clone());
    let rt = Arc::new(rt);
    router.set_mesh_runtime(rt.clone());
    if with_ingress {
        rt.set_leaf_ingress(ingress.clone()).ok().unwrap();
    }
    Rig { _dir: dir, kernel, rt, ingress, leaf, cert, id }
}

impl Rig {
    fn frame(&self, seq: u64, body: &[u8]) -> SignedPublish {
        SignedPublish::sign(self.cert.clone(), &self.leaf, seq, "t".into(), body.to_vec()).unwrap()
    }
    fn now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() }
    fn accept(&self, seq: u64, body: &[u8]) {
        let f = self.frame(seq, body);
        self.ingress.prepare(&f, Self::now()).expect("accepted");
        self.ingress.commit(&f).unwrap();
    }
    /// The full request path: capability check, then the handler.
    async fn call(&self, scope: &str, params: Value) -> Result<Value, (Option<String>, String)> {
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

#[tokio::test]
async fn a_non_admin_caller_is_refused_and_the_floor_stays() {
    let r = rig(90, true).await;
    for seq in 1..=44 { r.accept(seq, b"x"); }
    for scope in ["read", "write"] {
        let err = r.call(scope, json!({"leaf_id": r.id})).await.unwrap_err();
        assert!(err.1.contains("permission denied"), "{scope}: {err:?}");
    }
    assert_eq!(r.ingress.floor_of(&r.id).map(|f| f.0), Some(44));
    assert!(r.chain_events("leaf.floor.reset").await.is_empty());
}

#[tokio::test]
async fn a_connected_leaf_is_refused_and_the_floor_stays() {
    let r = rig(100, true).await;
    for seq in 1..=44 { r.accept(seq, b"x"); }
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
async fn a_disconnected_leaf_is_reset_recorded_and_stays_replay_protected() {
    let r = rig(110, true).await;
    for seq in 1..=44 { r.accept(seq, b"old"); }
    let old_digest = r.ingress.floor_of(&r.id).unwrap().1;
    assert!(r.ingress.prepare(&r.frame(1, b"new1"), Rig::now()).is_err(), "the reported refusal");

    let v = r.call("admin", json!({"leaf_id": r.id})).await.expect("reset");
    assert_eq!(v["reset"], true);
    assert_eq!(v["old_seq"], 44);
    assert_eq!(r.ingress.floor_of(&r.id), None);

    // Chain record: who, which leaf, the old floor, when.
    let ev = r.chain_events("leaf.floor.reset").await;
    assert_eq!(ev.len(), 1, "{ev:?}");
    assert_eq!(ev[0]["leaf_id"], r.id.as_str());
    assert_eq!(ev[0]["by"], "uid:501");
    assert_eq!(ev[0]["old_seq"], 44);
    let hex: String = old_digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(ev[0]["old_digest"], hex.as_str());
    assert!(chrono::DateTime::parse_from_rfc3339(ev[0]["at"].as_str().unwrap()).is_ok());

    // Recovery: the reflashed leaf's sequence from 1 is accepted.
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
