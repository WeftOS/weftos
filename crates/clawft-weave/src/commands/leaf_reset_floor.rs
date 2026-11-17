//! `weaver leaf reset-floor --leaf-id <id> --confirm`: let a reflashed leaf's
//! sequence restart at 1 (card e821c460). Owner/admin only, refused while the
//! leaf is connected or while its enrolled certificate is the one that wrote
//! the floor, recorded on the chain by the daemon.

use crate::protocol::Request;

/// The caller must say the pad was reflashed, before any request is sent.
pub fn require_confirm(confirm: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        confirm,
        "pass --confirm to state that this pad was reflashed and its sequence counter wiped; \
         resetting a pad that still holds its counter locks it out until its NVS is erased. \
         The leaf must also hold a re-issued certificate (new serial, same key) that is enrolled"
    );
    Ok(())
}

pub async fn run(leaf_id: &str, confirm: bool) -> anyhow::Result<()> {
    require_confirm(confirm)?;
    let mut client = clawft_rpc::connect_or_bail().await?;
    let resp = client
        .call(Request::with_params("leaf.reset_floor", serde_json::json!({ "leaf_id": leaf_id, "confirm": true })))
        .await?;
    if !resp.ok {
        let kind = resp.error_kind.as_deref().unwrap_or("error");
        anyhow::bail!("leaf.reset_floor refused ({kind}): {}", resp.error.unwrap_or_default());
    }
    let v = resp.result.unwrap_or_default();
    match (v["reset"].as_bool(), v["old_seq"].as_u64()) {
        (Some(true), Some(seq)) => {
            println!("leaf {leaf_id}: replay floor {seq} removed; its sequence may restart at 1");
            println!("recorded on the chain as leaf.floor.reset");
        }
        _ => println!("leaf {leaf_id}: no replay floor was stored; nothing to remove"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_floor_requires_the_confirm_flag() {
        assert!(require_confirm(false).unwrap_err().to_string().contains("--confirm"));
        assert!(require_confirm(true).is_ok());
    }
}
