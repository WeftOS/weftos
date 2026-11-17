//! `weaver leaf reset-floor --leaf-id <id>`: let a reflashed leaf's sequence
//! restart at 1 (card e821c460). Owner/admin only, refused while the leaf is
//! connected, recorded on the chain by the daemon.

use crate::protocol::Request;

pub async fn run(leaf_id: &str) -> anyhow::Result<()> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client
        .call(Request::with_params("leaf.reset_floor", serde_json::json!({ "leaf_id": leaf_id })))
        .await?;
    if !resp.ok {
        let kind = resp.error_kind.as_deref().unwrap_or("error");
        anyhow::bail!("leaf.reset_floor refused ({kind}): {}", resp.error.unwrap_or_default());
    }
    let v = resp.result.unwrap_or_default();
    match v["old_seq"].as_u64() {
        Some(seq) => println!("leaf {leaf_id}: replay floor {seq} removed; its sequence may restart at 1"),
        None => println!("leaf {leaf_id}: no replay floor was stored; nothing to remove"),
    }
    println!("recorded on the chain as leaf.floor.reset");
    Ok(())
}
