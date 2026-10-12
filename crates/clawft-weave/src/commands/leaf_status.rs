//! `weaver leaf status --leaf-id <id>`: is the leaf connected to this
//! daemon, and who holds its display (ADR-117).

use crate::protocol::Request;

pub async fn run(leaf_id: &str) -> anyhow::Result<()> {
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client
        .call(Request::with_params("display.status", serde_json::json!({ "leaf_id": leaf_id })))
        .await?;
    if !resp.ok {
        anyhow::bail!("display.status failed: {}", resp.error.unwrap_or_default());
    }
    let v = resp.result.unwrap_or_default();
    let yes_no = |b: bool| if b { "yes" } else { "no" };
    println!("leaf:       {leaf_id}");
    println!("connected:  {}", yes_no(v["connected"].as_bool().unwrap_or(false)));
    match v["session"].as_str() {
        Some(who) => println!("session:    held by {who}"),
        None => println!("session:    none"),
    }
    Ok(())
}
