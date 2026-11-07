//! `agent.sources` over the real signed `workload.ctl` wire between two
//! in-process nodes (the in-memory transport the kernel's placement tests use).
//! The host's agent data is a stub; the daemon's own answer needs a kernel.

use std::sync::Arc;

use async_trait::async_trait;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::node_facts_advert::sign_node_facts;
use clawft_kernel::workload_ctl::{MeshConnector, NodeAdmin, PlacementControlPlane, WorkloadHostService};
use clawft_kernel::workload_governance::WorkloadGate;
use clawft_kernel::workload_pkg::TrustAnchors;
use clawft_kernel::{ArtifactExchange, ArtifactStore, ExchangeConfig, node_id_from_pubkey};
use clawft_types::placement::{NodeFacts, TrustTier};
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};

use crate::agent_sources_rpc::METHOD;

struct Stub;

#[async_trait]
impl NodeAdmin for Stub {
    async fn call(&self, method: &str, _requester: &str, _body: &Value) -> Result<Value, String> {
        match method {
            METHOD => Ok(json!({
                "node_id": "stub",
                "agents": [{"pid": 4, "agent_id": "developer", "state": "running"}],
                "exposures": {"exposures": []},
            })),
            other => Err(format!("{other} unexpected")),
        }
    }
}

fn exchange(id: &str, chain: &Arc<ChainManager>) -> Arc<ArtifactExchange> {
    let mut ex = ArtifactExchange::new(id, Arc::new(ArtifactStore::new_memory()), ExchangeConfig::default()).unwrap();
    ex.set_chain_manager(chain.clone());
    Arc::new(ex)
}

fn gate(chain: &Arc<ChainManager>) -> Arc<WorkloadGate> {
    Arc::new(WorkloadGate::exempt(0.95, false, "test").with_chain(chain.clone()))
}

async fn mesh(admin: Option<Arc<dyn NodeAdmin>>, controller_listed: bool, tier: TrustTier) -> Result<(PlacementControlPlane, String), String> {
    let ctl_key = SigningKey::from_bytes(&[81; 32]);
    let host_key = SigningKey::from_bytes(&[82; 32]);
    let host_id = node_id_from_pubkey(&host_key.verifying_key().to_bytes());
    let host_chain = Arc::new(ChainManager::new(0, 1000));
    let controllers = if controller_listed { vec![ctl_key.verifying_key().to_bytes()] } else { vec![[9u8; 32]] };
    let svc = WorkloadHostService::new(host_key.clone(), exchange(&host_id, &host_chain), TrustAnchors::default(), gate(&host_chain))
        .with_controllers(controllers)
        .with_chain(host_chain.clone());
    let now = chrono::Utc::now().timestamp() as u64;
    svc.set_facts(sign_node_facts(&NodeFacts::new(host_id.clone(), now, 600, 1), &host_key).unwrap());
    if let Some(a) = admin {
        assert!(svc.set_node_admin(a));
    }
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("host", Arc::new(svc));
    let ctl_chain = Arc::new(ChainManager::new(0, 1000));
    let ctl_id = node_id_from_pubkey(&ctl_key.verifying_key().to_bytes());
    let plane = PlacementControlPlane::new(ctl_key, gate(&ctl_chain), ctl_chain.clone(), exchange(&ctl_id, &ctl_chain), TrustAnchors::default(), conn);
    plane.add_target(&addr, tier).await.map_err(|e| e.to_string())?;
    Ok((plane, host_id))
}

#[tokio::test]
async fn a_controller_reads_a_peers_agents_without_a_pinned_tier() {
    // Read-only: a merely paired peer may be asked (unlike a mutating method).
    let (plane, host) = mesh(Some(Arc::new(Stub)), true, TrustTier::Paired).await.unwrap();
    let out = plane.node_admin(&host, METHOD, json!({})).await.unwrap();
    assert_eq!(out["agents"][0]["agent_id"], "developer");
}

#[tokio::test]
async fn a_peer_that_does_not_list_this_node_as_a_controller_refuses() {
    // Refused when the controller is introduced, or at the call: never answered.
    let refused = match mesh(Some(Arc::new(Stub)), false, TrustTier::Pinned).await {
        Err(_) => true,
        Ok((plane, host)) => plane.node_admin(&host, METHOD, json!({})).await.is_err(),
    };
    assert!(refused);
}

#[tokio::test]
async fn a_peer_without_the_hook_refuses_rather_than_answering_empty() {
    let (plane, host) = mesh(None, true, TrustTier::Pinned).await.unwrap();
    assert!(plane.node_admin(&host, METHOD, json!({})).await.is_err());
}

#[tokio::test]
async fn the_daemon_hook_routes_the_method_and_refuses_until_its_kernel_is_set() {
    let e = crate::dashboard_rpc::MeshAdmin.call(METHOD, "n", &json!({})).await.unwrap_err();
    assert!(e.contains("agent directory"), "must be this method's refusal, not the dashboard's: {e}");
}
