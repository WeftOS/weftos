//! The daemon-as-controller path end to end (review round 2): a booted
//! kernel (chain in a temp dir), `init`, the operator's policy files in a
//! temp runtime dir, and a second node's `workload-host` served over Noise
//! TCP. Everything goes through `dispatch`, as `weaver workload
//! place|explain|status` does: `build`, `workload-peers.json` tiers read on
//! every call, placement on the remote node, and revocation by editing the
//! peers file without a restart.
//!
//! Review round 3 adds, on the same path: peer trust bound to the node key,
//! no container variant for a node without a container adapter, the
//! persisted placement state, and a Seed on its operator-assigned node id
//! (`workload-seeds.json`, `workload.place {store_pin}`).
//!
//! This is the only test that initialises the module's process-wide state.

use std::path::Path;
use std::sync::Arc;

use clawft_kernel::Kernel;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::revocation::RevocationKind;
use clawft_kernel::node_facts_advert::sign_node_facts;
use clawft_kernel::workload_ctl::{WorkloadHostService, listen_tcp, serve_listener};
use clawft_kernel::workload_ctl::OperatorPeer;
use clawft_kernel::workload_governance::{
    NetworkPolicy, NodeTrustTier, PackageTrust, WorkloadPermitRule,
};
use clawft_kernel::workload_pkg::codec::hex_encode;
use clawft_kernel::workload_pkg::key_id_for;
use clawft_kernel::workload_runtime::native::host_arch;
use clawft_kernel::workload_runtime::{NativeConfig, NativeRuntime, WorkloadHost};
use clawft_platform::NativePlatform;
use clawft_types::config::{ChainConfig, Config, KernelConfig};
use clawft_types::placement::{AttrValue, Capability, NodeFacts};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use super::tests::{anchors, cap, exchange, gate, package};
use super::*;
use crate::workload_place_policy::SEEDS_FILE;

fn native_caps(os: &str, class: &str) -> Vec<Capability> {
    let arch = host_arch().unwrap();
    vec![
        cap(&format!("cpu.arch.{arch}")),
        cap(&format!("os.{os}")),
        cap(&format!("node.class.{class}")),
        cap("runtime.native").with_attr(
            "arches_native",
            AttrValue::List(vec![AttrValue::from(arch)]),
        ),
        cap("mem.system").with_attr("free", 1i64 << 32),
    ]
}

fn signed_facts(
    key: &SigningKey,
    caps: Vec<Capability>,
) -> clawft_kernel::node_facts_advert::SignedNodeFacts {
    let id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let mut f = NodeFacts::new(id, chrono::Utc::now().timestamp() as u64, 600, 1);
    f.capabilities = caps;
    sign_node_facts(&f, key).unwrap()
}

/// A Linux board serving `workload-host` over Noise TCP to `controller`,
/// trusting packages from `signer`. Returns (node id, address, its chain).
async fn board(
    root: &Path,
    controller: &SigningKey,
    signer: &SigningKey,
) -> (String, String, Arc<ChainManager>) {
    let key = SigningKey::from_bytes(&[7; 32]);
    let id = clawft_kernel::node_id_from_pubkey(&key.verifying_key().to_bytes());
    let chain = Arc::new(ChainManager::new(0, 1000));
    let g = gate(&chain);
    let rt = NativeRuntime::new(NativeConfig {
        root: root.join("board-inst"),
        run_as: None,
        allow_interpreted: true,
    });
    let host = WorkloadHost::new(Arc::new(rt), g.clone(), id.clone(), NodeTrustTier::Paired)
        .with_chain(chain.clone());
    let svc = WorkloadHostService::new(key.clone(), exchange(&chain), anchors(signer), g)
        .with_route("native", Arc::new(host))
        .with_controllers(vec![controller.verifying_key().to_bytes()])
        .with_chain(chain.clone());
    svc.set_facts(signed_facts(&key, native_caps("linux", "pi5")));
    let listener = listen_tcp("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(serve_listener(listener, Arc::new(svc), true));
    (id, addr, chain)
}

/// A stand-in Cognitum Seed HTTP API (`fall-detect` 1.0.0 installed,
/// stopped). Returns its base URL and the count of `start` calls.
async fn fake_seed() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let n = starts.clone();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            let mut buf = vec![0u8; 8192];
            let len = s.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..len]).to_string();
            let line = req.lines().next().unwrap_or("").to_string();
            let (code, body) = if line.starts_with("GET /api/v1/apps/available") {
                (200, json!({"cogs": [{"id": "fall-detect", "version": "1.0.0"}]}))
            } else if line.starts_with("GET /api/v1/apps ") {
                (200, json!({"installed": [{"id": "fall-detect", "version": "1.0.0", "running": false}]}))
            } else if line.starts_with("POST /api/v1/apps/fall-detect/start") {
                n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                (200, json!({"ok": true}))
            } else {
                (404, json!({"error": "not found"}))
            };
            let b = body.to_string();
            let resp = format!(
                "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}",
                b.len()
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });
    (url, starts)
}

fn seed_token(runtime: &Path, node: &str) {
    use std::os::unix::fs::PermissionsExt;
    let d = runtime.join("secrets/workload.seed");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::set_permissions(runtime.join("secrets"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
    let f = d.join(format!("{node}.token"));
    std::fs::write(&f, "seed-token-0123456789abcdef").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn placed_ids(runtime: &Path) -> Vec<String> {
    let v: Value =
        serde_json::from_slice(&std::fs::read(runtime.join(STATE_FILE)).unwrap()).unwrap();
    v["placements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["instance_id"].as_str().unwrap().to_string())
        .collect()
}

fn write(dir: &Path, name: &str, v: &Value) {
    std::fs::write(dir.join(name), serde_json::to_vec(v).unwrap()).unwrap();
}

async fn call(
    kernel: &Arc<RwLock<Kernel<NativePlatform>>>,
    m: &str,
    p: Value,
) -> Result<Value, String> {
    let r = dispatch(m, p, kernel.clone()).await;
    if r.ok {
        Ok(r.result.unwrap_or(Value::Null))
    } else {
        Err(r.error.unwrap_or_default())
    }
}

fn tier_of(status: &Value, node: &str) -> String {
    status["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["node_id"] == node)
        .map(|t| t["tier"].as_str().unwrap().to_string())
        .unwrap_or_default()
}

/// Hex sha256 of a test's own temp dir: the per-run identity behind every
/// revocation this test makes. A booted kernel loads its revocation list from
/// the runtime root every test process (and every `cargo test` run that reuses
/// `WEFTOS_RUNTIME_DIR`) shares, so revoking a fixed key or name here would
/// refuse it in every other kernel, such as the service-mode placement
/// integration test, and make a rerun see it as already revoked.
fn run_digest(tmp: &Path) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(tmp.to_string_lossy().as_bytes()).into()
}

/// The operator signer this run packs, trusts and later revokes.
fn revoked_signer_for(tmp: &Path) -> SigningKey {
    SigningKey::from_bytes(&run_digest(tmp))
}

/// A package name only this run revokes.
fn revoked_name_for(tmp: &Path, what: &str) -> String {
    format!("cog.{what}-probe-{}", &hex_encode(&run_digest(tmp))[..16])
}

#[tokio::test]
async fn weaver_place_goes_through_the_daemon_and_peer_tiers_are_live_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let runtime = tmp.path().join("runtime");
    std::fs::create_dir_all(&runtime).unwrap();
    let kcfg = KernelConfig {
        chain: Some(ChainConfig::isolated_in(&tmp.path().join("chain"))),
        ..KernelConfig::default()
    };
    let kernel = Kernel::boot(Config::default(), kcfg, Arc::new(NativePlatform::new()))
        .await
        .expect("kernel boots");
    let node_key = SigningKey::from_bytes(&[5; 32]);
    let signer = revoked_signer_for(tmp.path());
    // The package names this run revokes are its own too: a rerun against the
    // same runtime root must still see them as newly revoked.
    let early_pkg = revoked_name_for(tmp.path(), "early");
    let after_push_pkg = revoked_name_for(tmp.path(), "after-push");
    let me = clawft_kernel::node_id_from_pubkey(&node_key.verifying_key().to_bytes());
    // This node is the dev Mac: its own re-probed signed facts.
    kernel
        .cluster_membership()
        .facts()
        .insert(
            signed_facts(&node_key, {
                // As the macOS probe reports it with Docker installed.
                let mut c = native_caps("macos", "dev-mac");
                c.push(cap("runtime.container.docker").with_attr(
                    "arches_native",
                    AttrValue::List(vec![AttrValue::from(host_arch().unwrap())]),
                ));
                c
            }),
            TrustTier::Pinned,
            chrono::Utc::now().timestamp() as u64,
        )
        .unwrap();
    let kernel = Arc::new(RwLock::new(kernel));

    // Operator policy, as an operator writes it into the runtime dir.
    let mut permit = WorkloadPermitRule::new("operator-cog", ["workload.*"], ["cog"]);
    permit.max_network = NetworkPolicy::Egress;
    let mut seed_permit = WorkloadPermitRule::new("operator-seed", ["workload.*"], ["cog"]);
    seed_permit.min_package_trust = PackageTrust::OperatorAttested;
    seed_permit.max_network = NetworkPolicy::Egress;
    write(&runtime, PERMITS_FILE, &json!([permit, seed_permit]));
    let (seed_url, seed_starts) = fake_seed().await;
    write(
        &runtime,
        SEEDS_FILE,
        &json!([{ "node_id": "seed-lab", "url": seed_url, "tier": "paired",
                  "allow_unpinned_lab_link": true,
                  "pins": [{ "id": "fall-detect", "version": "1.0.0" }] }]),
    );
    seed_token(&runtime, "seed-lab");
    let spk = signer.verifying_key().to_bytes();
    write(
        &runtime,
        TRUST_FILE,
        &json!({ "schema": "weftos.workload-trust.v1",
        "operator_keys": [{ "key_id": key_id_for(&spk), "public_key": hex_encode(&spk) }] }),
    );
    let (pi, pi_addr, pi_chain) = board(tmp.path(), &node_key, &signer).await;
    let paired = json!([{ "addr": pi_addr, "tier": "paired" }]);
    write(&runtime, PEERS_FILE, &paired);
    init(node_key.clone(), runtime.clone());

    // Incident: the permits file is broken, so the placement plane cannot be
    // built. Revoking a leaked key must still work, against the kernel's
    // list alone, and be chained. (The plane is built, and the file fixed,
    // below.)
    let good_permits = std::fs::read(runtime.join(PERMITS_FILE)).unwrap();
    std::fs::write(runtime.join(PERMITS_FILE), "{broken").unwrap();
    let early = call(&kernel, "workload.revoke", json!({ "package": early_pkg }))
        .await
        .unwrap();
    assert_eq!(early["newly_revoked"], true, "{early}");
    assert!(
        call(&kernel, "workload.status", json!({})).await.unwrap_err().contains("placement unavailable"),
        "the plane really could not be built"
    );
    assert!(
        kernel
            .read()
            .await
            .revocation_list()
            .is_subject_revoked(RevocationKind::Package, &early_pkg)
    );
    std::fs::write(runtime.join(PERMITS_FILE), good_permits).unwrap();

    // After a governance push the plane refuses until restart; the verb does not.
    let stale = Some("an-older-hash".to_string());
    let r = dispatch_checked("workload.status", json!({}), kernel.clone(), Some(&stale)).await;
    assert!(!r.ok && r.error.unwrap().contains("governance changed"));
    let r = dispatch_checked(
        "workload.revoke",
        json!({ "package": after_push_pkg }),
        kernel.clone(),
        Some(&stale),
    )
    .await;
    assert!(r.ok, "{:?}", r.error);

    let pkg = package(&tmp.path().join("pkgsrc"), &signer);
    let params = json!({ "package_dir": pkg, "mode": "listener", "csi_port": 15027 });

    // explain: the ARM board over this Mac, and why.
    let v = call(&kernel, "workload.explain", params.clone())
        .await
        .unwrap();
    let explain = v["explain"].as_str().unwrap();
    assert!(explain.contains(&format!("PLACED on {pi}")), "{explain}");
    assert!(explain.contains(&me), "the Mac was considered: {explain}");
    // The Mac probes Docker but serves no container adapter (no
    // workload-container.json): it is not offered a container variant.
    assert!(!explain.contains(&format!("{me} eligible via")), "{explain}");
    let st = call(&kernel, "workload.status", json!({})).await.unwrap();
    assert_eq!(tier_of(&st, &pi), "paired");
    assert_eq!(tier_of(&st, &me), "pinned");
    assert_eq!(st["workload_host"]["metadata"]["routes"], "native");

    // The in-process host the daemon builds holds the kernel's own list.
    assert!(
        HOST.get().unwrap().revocations().is_some_and(|l| Arc::ptr_eq(
            l,
            kernel.try_read().unwrap().revocation_list()
        )),
        "build() gives the workload-host the kernel's revocation list"
    );

    // Trust is bound to the key: the peer listed for another key loses
    // its tier, and the listed key is not learned (nobody holds it here).
    let other = hex_encode(&SigningKey::from_bytes(&[8; 32]).verifying_key().to_bytes());
    write(&runtime, PEERS_FILE, &json!([{ "addr": pi_addr, "tier": "paired", "key": other }]));
    let st = call(&kernel, "workload.status", json!({})).await.unwrap();
    assert_eq!(tier_of(&st, &pi), "discovered");
    let pi_key = hex_encode(&SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes());
    write(&runtime, PEERS_FILE, &json!([{ "addr": pi_addr, "tier": "paired", "key": pi_key }]));
    let st = call(&kernel, "workload.status", json!({})).await.unwrap();
    assert_eq!(tier_of(&st, &pi), "paired");

    // The operator drops the peer: revoked on the next call, no restart.
    write(&runtime, PEERS_FILE, &json!([]));
    let v = call(&kernel, "workload.explain", params.clone())
        .await
        .unwrap();
    assert!(
        !v["explain"]
            .as_str()
            .unwrap()
            .contains(&format!("PLACED on {pi}"))
    );
    let st = call(&kernel, "workload.status", json!({})).await.unwrap();
    assert_eq!(tier_of(&st, &pi), "discovered");

    // A request naming it cannot raise it back.
    let mut named = params.clone();
    named["peers"] = json!([pi_addr]);
    let v = call(&kernel, "workload.explain", named).await.unwrap();
    assert!(
        !v["explain"]
            .as_str()
            .unwrap()
            .contains(&format!("PLACED on {pi}"))
    );

    // A broken peers file fails closed.
    std::fs::write(runtime.join(PEERS_FILE), "{broken").unwrap();
    assert!(
        call(&kernel, "workload.explain", params.clone())
            .await
            .unwrap_err()
            .contains(PEERS_FILE)
    );

    // Listed as paired again: place runs the cog on the board.
    write(&runtime, PEERS_FILE, &paired);
    let v = call(&kernel, "workload.place", params.clone()).await.unwrap();
    assert_eq!(v["placed"]["node_id"], pi.as_str(), "{}", v["explain"]);
    let iid = v["placed"]["instance_id"].as_str().unwrap().to_string();
    let st = call(&kernel, "workload.status", json!({ "instance_id": iid }))
        .await
        .unwrap();
    assert_eq!(st["status"]["state"], "running");
    assert_eq!(placed_ids(&runtime), vec![iid.clone()], "persisted");
    assert!(
        pi_chain
            .tail(pi_chain.len())
            .iter()
            .any(|e| e.kind == "workload.place"),
        "placed on the board's own chain"
    );

    // `weaver workload revoke --signer`: nothing of it runs on THIS node (the
    // instance is on the board) so nothing local is torn down, and with no
    // mesh no notice goes out (the answer says so). The daemon's placement
    // gate and exchange have the kernel's revocation list, so the package is
    // now refused;
    // the revocation is chained by the list at boot; and the instance on the
    // board can still be taken down by hand.
    let signer_hex = hex_encode(&signer.verifying_key().to_bytes());
    let r = call(
        &kernel,
        "workload.revoke",
        json!({ "signer": signer_hex, "reason": "leaked" }),
    )
    .await
    .unwrap();
    assert_eq!(r["newly_revoked"], true, "{r}");
    assert_eq!(r["forced"], json!([]), "{r}");
    assert!(r["notice"].as_str().unwrap().contains("no mesh"), "{r}");
    // Refused at verify/seed time (the exchange has the same list), before
    // any node is asked.
    let e = call(&kernel, "workload.explain", params).await.unwrap_err();
    assert!(e.contains("is revoked"), "{e}");
    assert!(
        kernel
            .read()
            .await
            .chain_manager()
            .unwrap()
            .tail(10_000)
            .iter()
            .any(|e| e.kind == "workload.revoke"
                && e.payload.as_ref().is_some_and(|p| p["revoked_by"] == "operator")),
        "the revocation is on the chain"
    );
    // Admin only, one subject at a time.
    assert!(call(&kernel, "workload.revoke", json!({})).await.is_err());

    call(&kernel, "workload.stop", json!({ "instance_id": iid }))
        .await
        .unwrap();
    call(&kernel, "workload.unload", json!({ "instance_id": iid }))
        .await
        .unwrap();
    assert!(placed_ids(&runtime).is_empty(), "the unload is persisted");

    // A Seed on its operator-assigned node id, through the daemon.
    let pin = json!({ "store_pin": { "node_id": "seed-lab", "id": "fall-detect",
                                     "version": "1.0.0", "mode": "interval" } });
    let v = call(&kernel, "workload.place", pin).await.unwrap();
    assert_eq!(v["route"], "remote.api");
    assert_eq!(v["placed"]["node_id"], "seed-lab");
    assert_eq!(seed_starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let st = call(&kernel, "workload.status", json!({})).await.unwrap();
    assert!(
        st["instances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["placement"]["node_id"] == "seed-lab"),
        "{st}"
    );
    let unknown = json!({ "store_pin": { "node_id": "seed-nowhere", "id": "fall-detect",
                                         "version": "1.0.0" } });
    let e = call(&kernel, "workload.place", unknown).await.unwrap_err();
    assert!(e.contains("no Seed adapter"), "{e}");
}

#[test]
fn peer_entries_are_validated_at_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_peers(dir.path()).unwrap().is_empty());
    for bad in [
        json!([{ "addr": "mem://local", "tier": "paired" }]),
        json!([{ "addr": "no-port", "tier": "paired" }]),
        json!([{ "addr": "a b:1", "tier": "paired" }]),
        json!([{ "addr": "h:1", "tier": "paired", "extra": 1 }]),
        json!([{ "addr": "h:1", "tier": "root" }]),
    ] {
        write(dir.path(), PEERS_FILE, &bad);
        assert!(load_peers(dir.path()).is_err(), "{bad}");
    }
    write(
        dir.path(),
        PEERS_FILE,
        &json!([{ "addr": "h:1" }, { "addr": "[::1]:2", "tier": "discovered" }]),
    );
    assert_eq!(
        load_peers(dir.path()).unwrap(),
        vec![
            OperatorPeer::new("h:1", TrustTier::Paired),
            OperatorPeer::new("[::1]:2", TrustTier::Discovered)
        ]
    );
    let k = [9u8; 32];
    write(dir.path(), PEERS_FILE, &json!([{ "addr": "h:1", "key": hex_encode(&k) }]));
    assert_eq!(
        load_peers(dir.path()).unwrap(),
        vec![OperatorPeer::new("h:1", TrustTier::Paired).with_key(k)]
    );
    write(dir.path(), PEERS_FILE, &json!([{ "addr": "h:1", "key": "abc" }]));
    assert!(load_peers(dir.path()).is_err());
}

#[test]
fn container_and_seed_files_are_validated_at_the_boundary() {
    use crate::workload_place_policy::{CONTAINER_FILE, load_container};
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("w");
    assert!(load_container(dir.path(), &work).unwrap().is_none());
    let digest = format!("debian@sha256:{}", "a".repeat(64));
    for bad in [
        json!({ "engine": "lxc", "base_image": digest }),
        json!({ "engine": "docker", "base_image": "debian:latest" }),
        json!({ "engine": "docker", "base_image": digest, "arches_native": ["mips"] }),
        json!({ "engine": "docker", "base_image": digest, "extra": 1 }),
    ] {
        write(dir.path(), CONTAINER_FILE, &bad);
        assert!(load_container(dir.path(), &work).is_err(), "{bad}");
    }
    write(dir.path(), CONTAINER_FILE, &json!({ "engine": "docker", "base_image": digest }));
    let cfg = load_container(dir.path(), &work).unwrap().unwrap();
    assert_eq!(cfg.arches_native, vec!["aarch64".to_string()]);
    assert!(!cfg.allow_emulated);

    let chain = Arc::new(ChainManager::new(0, 10));
    let g = gate(&chain);
    for bad in [
        json!([{ "node_id": "bad id", "url": "http://s:1", "pins": [] }]),
        json!([{ "node_id": "s", "url": "ftp://s", "pins": [] }]),
        json!([{ "node_id": "s", "url": "http://s:1", "tls_sha256": "sha256:00", "pins": [] }]),
        json!([{ "node_id": "s", "url": "http://s:1", "pins": [{ "id": "x", "version": "" }] }]),
    ] {
        write(dir.path(), SEEDS_FILE, &bad);
        assert!(crate::workload_place_policy::load_seeds(dir.path(), g.clone(), &chain).is_err(), "{bad}");
    }
}
