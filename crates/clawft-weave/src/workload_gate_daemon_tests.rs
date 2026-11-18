//! The daemon refuses ungoverned workload actions for real: a booted kernel
//! (real chain, shipped default-deny rules, its revocation list) decides
//! `workload.install` / `workload.unload` through the operator's
//! `workload-permits.json`. No permit: denied and chained. A permit:
//! permitted and chained. A revoked package: denied either way. Everything
//! goes through the daemon's own `dispatch` path; the chain is the one
//! `weaver chain` reads.

use std::path::Path;
use std::sync::Arc;

use clawft_kernel::Kernel;
use clawft_kernel::chain::ChainEvent;
use clawft_kernel::revocation::RevocationKind;
use clawft_kernel::workload_governance::{
    CATALOG_PRINCIPAL, NodeTrustTier, PackageTrust, WorkloadPermitRule,
};
use clawft_platform::NativePlatform;
use clawft_types::config::{ChainConfig, Config, KernelConfig};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use super::*;
use crate::workload_place_policy::PERMITS_FILE;

type K = Arc<RwLock<Kernel<NativePlatform>>>;

async fn boot(root: &Path) -> K {
    let kcfg = KernelConfig {
        chain: Some(ChainConfig::isolated_in(&root.join("chain"))),
        ..KernelConfig::default()
    };
    let k = Kernel::boot(Config::default(), kcfg, Arc::new(NativePlatform::new()))
        .await
        .expect("kernel boots");
    Arc::new(RwLock::new(k))
}

async fn call(k: &K, dir: &Path, m: &str, p: Value) -> Result<Value, String> {
    let r = dispatch_in(m, p, k.clone(), Some(dir)).await;
    if r.ok {
        Ok(r.result.unwrap_or(Value::Null))
    } else {
        Err(r.error.unwrap_or_default())
    }
}

async fn events(k: &K, kind: &str) -> Vec<ChainEvent> {
    let k = k.read().await;
    let cm = k.chain_manager().unwrap();
    cm.tail(cm.len()).into_iter().filter(|e| e.kind == kind).collect()
}

fn permit(actions: &[&str], trust: PackageTrust) -> Value {
    let mut p = WorkloadPermitRule::new("operator-catalog", actions.iter().copied(), ["cog"]);
    p.min_package_trust = trust;
    p.min_node_tier = NodeTrustTier::Pinned;
    if trust == PackageTrust::Unsigned {
        // Required: a permit that accepts unsigned packages names who it is for.
        p.principals = vec![CATALOG_PRINCIPAL.into()];
    }
    json!([p])
}

/// The manifest hash no test ever revokes.
fn clean_hash() -> String {
    "ab".repeat(32)
}

fn install(name: &str) -> Value {
    install_with_hash(name, &clean_hash())
}

fn install_with_hash(name: &str, hash: &str) -> Value {
    json!({ "name": name, "kind": "cog", "version": "1.0.0",
            "manifest_hash": format!("blake3:{hash}") })
}

/// An artifact hash that only this test run revokes. A booted kernel loads
/// its revocation list from the runtime root the whole test process shares
/// (and `WEFTOS_RUNTIME_DIR` can outlive the process), so a revocation written
/// here is visible to every other kernel the process boots. Deriving the hash
/// from this test's own temp dir keeps it from denying their installs.
fn revoked_hash_for(tmp: &Path) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(tmp.to_string_lossy().as_bytes()))
}

fn decisions(evs: &[ChainEvent], name_or_action: &str) -> Vec<String> {
    evs.iter()
        .filter(|e| e.source == "workload")
        .filter_map(|e| e.payload.as_ref())
        .filter(|p| p["action"] == name_or_action)
        .map(|p| p["decision"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test]
async fn install_is_denied_and_chained_without_a_permit_and_permitted_and_chained_with_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runtime");
    std::fs::create_dir_all(&dir).unwrap();
    let k = boot(tmp.path()).await;

    // No permits file: default deny. The refusal says why, the gate chained
    // its denial under the action's own kind, and nothing was recorded.
    let e = call(&k, &dir, "workload.install", install("gate-probe-a")).await.unwrap_err();
    assert!(e.contains("default deny"), "{e}");
    let evs = events(&k, "workload.install").await;
    assert_eq!(decisions(&evs, "workload.install"), ["deny"], "{evs:?}");
    assert!(!events(&k, "workload.refuse").await.is_empty(), "the refusal is chained too");
    let list = call(&k, &dir, "workload.list", json!({})).await.unwrap();
    assert!(list.to_string().find("gate-probe-a").is_none(), "{list}");

    // A permit for install lifts the default deny (the catalog verifies
    // nothing, so the permit has to accept unsigned packages).
    std::fs::write(
        dir.join(PERMITS_FILE),
        serde_json::to_vec(&permit(&["workload.install", "workload.unload"], PackageTrust::Unsigned)).unwrap(),
    )
    .unwrap();
    let v = call(&k, &dir, "workload.install", install("gate-probe-b")).await.unwrap();
    assert_eq!(v["name"], "gate-probe-b");
    let evs = events(&k, "workload.install").await;
    assert_eq!(decisions(&evs, "workload.install"), ["deny", "permit"]);
    let permitted = evs
        .iter()
        .filter_map(|e| e.payload.as_ref())
        .find(|p| p["decision"] == "permit")
        .unwrap();
    assert_eq!(permitted["permit_rule"], "operator-catalog");
    // The install itself is chained after the permit.
    assert!(evs.iter().any(|e| e.payload.as_ref().is_some_and(|p| p["name"] == "gate-probe-b")));
    let list = call(&k, &dir, "workload.list", json!({})).await.unwrap();
    assert!(list.to_string().contains("gate-probe-b"));

    // A permit that wants a signed package does not cover the unverified catalog.
    std::fs::write(
        dir.join(PERMITS_FILE),
        serde_json::to_vec(&permit(&["workload.install"], PackageTrust::PinnedSigner)).unwrap(),
    )
    .unwrap();
    let e = call(&k, &dir, "workload.install", install("gate-probe-c")).await.unwrap_err();
    assert!(e.contains("default deny"), "{e}");

    // An unsigned-floor permit that names no principal is refused outright
    // (it would otherwise admit any caller), and fails closed.
    let mut open = permit(&["workload.install"], PackageTrust::Unsigned);
    open[0].as_object_mut().unwrap().remove("principals");
    std::fs::write(dir.join(PERMITS_FILE), serde_json::to_vec(&open).unwrap()).unwrap();
    let e = call(&k, &dir, "workload.install", install("gate-probe-c2")).await.unwrap_err();
    assert!(e.contains("fail closed") && e.contains("principals"), "{e}");
    std::fs::write(
        dir.join(PERMITS_FILE),
        serde_json::to_vec(&permit(&["workload.install"], PackageTrust::PinnedSigner)).unwrap(),
    )
    .unwrap();

    // Unload is governed too: allowed by its permit, denied without one.
    let e = call(&k, &dir, "workload.unload", json!({ "name": "gate-probe-b" })).await.unwrap_err();
    assert!(e.contains("default deny"), "{e}");
    std::fs::write(
        dir.join(PERMITS_FILE),
        serde_json::to_vec(&permit(&["workload.install", "workload.unload"], PackageTrust::Unsigned)).unwrap(),
    )
    .unwrap();
    call(&k, &dir, "workload.unload", json!({ "name": "gate-probe-b" })).await.unwrap();
    assert_eq!(
        decisions(&events(&k, "workload.unload").await, "workload.unload"),
        ["deny", "permit"]
    );
}

#[tokio::test]
async fn a_revoked_package_is_denied_even_with_a_permit_and_a_broken_policy_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runtime");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(PERMITS_FILE),
        serde_json::to_vec(&permit(&["workload.install"], PackageTrust::Unsigned)).unwrap(),
    )
    .unwrap();
    let k = boot(tmp.path()).await;
    let revoked = revoked_hash_for(tmp.path());
    assert_ne!(revoked, clean_hash(), "the revoked hash must not be one other tests install");

    // By package name and by the manifest's artifact hash.
    k.read().await.revocation_list().revoke_subject(RevocationKind::Package, "gate-probe-d", "bad").unwrap();
    let e = call(&k, &dir, "workload.install", install("gate-probe-d")).await.unwrap_err();
    assert!(e.contains("is revoked"), "{e}");
    call(&k, &dir, "workload.install", install("gate-probe-e")).await.unwrap();
    k.read()
        .await
        .revocation_list()
        .revoke_subject(RevocationKind::ArtifactHash, &revoked, "bad")
        .unwrap();
    let e = call(&k, &dir, "workload.install", install_with_hash("gate-probe-f", &revoked))
        .await
        .unwrap_err();
    assert!(e.contains("is revoked"), "{e}");
    // Both revocations were chained by the list itself.
    assert_eq!(events(&k, "workload.revoke").await.len(), 2);

    // A permits file that does not parse fails closed (and says so).
    std::fs::write(dir.join(PERMITS_FILE), "{broken").unwrap();
    let e = call(&k, &dir, "workload.install", install("gate-probe-g")).await.unwrap_err();
    assert!(e.contains("fail closed"), "{e}");
    // Without a policy directory the kernel's own gate decides, and it
    // default-denies workload.* (the shipped rule): never an open door.
    let r = dispatch_in("workload.install", install("gate-probe-h"), k.clone(), None).await;
    assert!(!r.ok && r.error.unwrap().contains("denied"));
    // Read-only verbs do not depend on the policy file.
    call(&k, &dir, "workload.list", json!({})).await.unwrap();
}

#[test]
fn the_local_names_are_the_kernels_chain_kinds() {
    assert_eq!(WORKLOAD_INSTALL, clawft_kernel::chain::EVENT_KIND_WORKLOAD_INSTALL);
    assert_eq!(WORKLOAD_UNLOAD, clawft_kernel::chain::EVENT_KIND_WORKLOAD_UNLOAD);
    assert_eq!(WORKLOAD_REFUSE, clawft_kernel::chain::EVENT_KIND_WORKLOAD_REFUSE);
}
