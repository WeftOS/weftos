//! `weaver mesh identity`, `journal verify` and `trust`.

use std::io::Write;

use anyhow::{anyhow, bail, Context, Result};
use clawft_mesh_local::hexser;
use clawft_mesh_local::proto::{Message, Role};
use clawft_mesh_service::admin_client::{fingerprint, write_pin};

use super::client::{call, connect, default_pin};
use super::render::s;
use super::{ConnArgs, IdentityArgs, TrustArgs};

/// `weaver mesh identity`: the public half of `<runtime>/node.key`, in the
/// `report.mesh_identity` shape the heartbeat already sends. Reads the key
/// file only to derive the public key; nothing about the daemon changes
/// and the seed never reaches `w` or `--out`.
pub(super) fn identity(w: &mut dyn Write, a: &IdentityArgs) -> Result<()> {
    let (pk, key_path, label) = if a.user {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
        let (key, source) = crate::user_key::resolve_user_key(&home, false)
            .map_err(|e| anyhow!("user key: {e}"))?;
        let path = match source {
            crate::user_key::KeySource::UserKey(p) | crate::user_key::KeySource::ChainKey(p) | crate::user_key::KeySource::Generated(p) => p,
        };
        (key.verifying_key().to_bytes(), path, "user")
    } else {
        let key_path = crate::protocol::runtime_paths().node_key();
        let bytes = std::fs::read(&key_path).with_context(|| {
            format!(
                "no node key at {} — point WEFTOS_RUNTIME_DIR at the daemon's runtime dir (`weaver kernel status` prints it)",
                key_path.display()
            )
        })?;
        if bytes.len() != 32 {
            bail!("node.key at {} is malformed (expected 32 bytes, got {})", key_path.display(), bytes.len());
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes);
        (ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key().to_bytes(), key_path, "node")
    };
    let id = crate::mesh_pair::MeshIdentity::from_pubkey(&pk, None);
    if let Some(out) = &a.out {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        let mut f = opts.open(out).with_context(|| format!("create {}", out.display()))?;
        f.write_all(&pk)?;
        f.sync_all()?;
    }
    if a.json {
        writeln!(w, "{}", serde_json::to_string_pretty(&id)?)?;
    } else {
        writeln!(w, "{label} id:     {}", id.node)?;
        if a.user {
            writeln!(w, "scope:       user:{}", id.node)?;
        }
        writeln!(w, "fingerprint: {}", id.fingerprint)?;
        writeln!(w, "ed25519:     {}", id.ed25519)?;
        writeln!(w, "key file:    {} (public half only)", key_path.display())?;
        if let Some(out) = &a.out {
            let hint = if a.user { "weaver leaf enroll --parent-pubkey" } else { "weaver leaf provision --mesh-pubkey" };
            writeln!(w, "raw pubkey:  {} (32 bytes, for `{hint}`)", out.display())?;
        }
    }
    Ok(())
}

pub(super) async fn journal_verify(w: &mut dyn Write, c: &ConnArgs, accept: bool, seq: Option<u64>, floor: Option<u64>) -> Result<()> {
    let v = call(c, Role::Admin, Message::JournalVerify {}).await?;
    if c.json {
        writeln!(w, "{}", serde_json::to_string_pretty(&v)?)?;
    } else if v["ok"] == true {
        writeln!(w, "journal verifies: {} records, head seq {} hash {}", v["records"], v["head_seq"], s(&v, "head_hash"))?;
    } else {
        writeln!(w, "JOURNAL DOES NOT VERIFY: {} ({})", v["bad"]["reason"], v["bad"]["file"])?;
    }
    if v["last_auto_accept"].is_object() {
        writeln!(
            w,
            "the service accepted a crash-torn tail on its own at seq {} (time {}); nothing readable was lost",
            v["last_auto_accept"]["seq"], v["last_auto_accept"]["at"]
        )?;
    }
    if v["read_only"] == true {
        writeln!(
            w,
            "the journal is read-only after a quarantined tail (pending quarantine seq {})",
            v["latest_pending_quarantine"]
        )?;
        if v["pending_quarantines"].as_array().is_some_and(|a| a.len() > 1) {
            writeln!(
                w,
                "{} quarantines are pending ({}); one acceptance clears them all: review every journal.corrupt.* file first and pass --floor if an older one holds a higher serial",
                v["pending_quarantines"].as_array().map_or(0, Vec::len),
                v["pending_quarantines"]
            )?;
        }
    }
    if accept {
        call(c, Role::Admin, Message::JournalAcceptTruncate { quarantine_seq: seq, floor }).await?;
        writeln!(w, "accepted the quarantined tail; binds and certificates are allowed again")?;
    }
    if v["ok"] != true {
        bail!("the machine journal does not verify");
    }
    Ok(())
}

pub(super) async fn trust(w: &mut dyn Write, t: TrustArgs) -> Result<()> {
    // No pin comparison here: the point is to show the key and record it.
    let cl = connect(&t.conn, Role::User, false).await?;
    let key = cl.ack.machine_pubkey;
    let (node, sha) = (cl.ack.node_id.clone(), cl.ack.service_build_sha.clone());
    cl.bye().await;
    writeln!(w, "service node {node} (build {sha})")?;
    writeln!(w, "machine key   {}", hexser::encode(&key))?;
    writeln!(w, "fingerprint   {}", fingerprint(&key))?;
    writeln!(w, "Compare this fingerprint out of band (`weaver mesh status` run on the service host as root or an admin shows the same one) before relying on the pin.")?;
    let path = t.conn.pin.clone().or_else(default_pin).context("no pin path: set --pin or HOME")?;
    write_pin(&path, &key, t.replace)?;
    writeln!(w, "pinned in {}", path.display())?;
    Ok(())
}
