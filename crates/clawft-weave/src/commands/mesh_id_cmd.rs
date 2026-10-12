//! `weaver mesh create | adopt-licence | id`: the mesh's `MeshId` (ADR-114 §2).
//! The work is in [`crate::mesh_genesis`]; this file reads the keys and the
//! config, calls it and prints. Nothing here prints key material.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use clawft_kernel::licence::{MeshId, mesh_id_from_config};
use ed25519_dalek::SigningKey;

use crate::mesh_genesis::{self, Resolved};

/// `weaver mesh id`.
#[derive(Args, Debug)]
pub struct IdArgs {
    /// Print JSON.
    #[arg(long)]
    pub json: bool,
}

/// The runtime dir and the Seed licence's id (from `kernel.mesh`), as the
/// daemon boots with them.
pub async fn local_state() -> Result<(PathBuf, Option<MeshId>)> {
    let dir = crate::protocol::runtime_paths().root().to_path_buf();
    let cfg = super::load_config(&clawft_platform::NativePlatform::new(), None).await.context("loading the config")?;
    let licence = match cfg.kernel.mesh.as_ref() {
        Some(m) => mesh_id_from_config(m.genesis_hash.as_deref(), m.mesh_nonce.as_deref()).map_err(|e| anyhow!("{e}"))?,
        None => None,
    };
    Ok((dir, licence))
}

/// This node's mesh id as the CLI sees it (`None`: there is none).
pub async fn local_mesh_id() -> Result<Option<MeshId>> {
    let (dir, licence) = local_state().await?;
    Ok(mesh_genesis::resolve(&dir, licence).map_err(anyhow::Error::msg)?.map(|r| r.id))
}

fn node_key(dir: &Path) -> Result<SigningKey> {
    let path = dir.join("node.key");
    let seed = crate::user_key::read_seed(&path).map_err(|e| {
        anyhow!("node key: {e} (point WEFTOS_RUNTIME_DIR at the daemon's runtime dir; `weaver kernel status` prints it)")
    })?;
    Ok(SigningKey::from_bytes(&seed))
}

fn user_key() -> Result<SigningKey> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("cannot determine the home directory"))?;
    let (key, _) = crate::user_key::resolve_user_key(&home, false).map_err(|e| anyhow!("user key: {e}"))?;
    Ok(key)
}

/// `weaver mesh create`.
pub async fn create(w: &mut dyn Write) -> Result<()> {
    let (dir, licence) = local_state().await?;
    let node = node_key(&dir)?;
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    create_with(w, &dir, &node, &user_key()?, licence, &now)
}

/// `create` over explicit inputs.
pub fn create_with(
    w: &mut dyn Write,
    dir: &Path,
    node: &SigningKey,
    user: &SigningKey,
    licence: Option<MeshId>,
    now: &str,
) -> Result<()> {
    let s = mesh_genesis::create(dir, node, user, licence, now).map_err(anyhow::Error::msg)?;
    writeln!(w, "mesh id: {}", s.record.mesh_id)?;
    writeln!(w, "record:  {}", mesh_genesis::record_path(dir).display())?;
    writeln!(
        w,
        "restart the daemon so it uses it; members learn it when they pair with this node (weftos://{}/...)",
        s.record.mesh_id
    )?;
    Ok(())
}

/// `weaver mesh adopt-licence`.
pub async fn adopt_licence(w: &mut dyn Write) -> Result<()> {
    let (dir, licence) = local_state().await?;
    let id = mesh_genesis::adopt_licence(&dir, licence).map_err(anyhow::Error::msg)?;
    writeln!(w, "mesh id: {} (from the Seed licence; the daemon uses it at boot)", id.to_hex())?;
    Ok(())
}

/// `weaver mesh id`.
pub async fn id(w: &mut dyn Write, a: &IdArgs) -> Result<()> {
    let (dir, licence) = local_state().await?;
    show(w, &dir, licence, a.json)
}

/// `id` over explicit inputs. No id is an error exit (after printing JSON, if asked).
pub fn show(w: &mut dyn Write, dir: &Path, licence: Option<MeshId>, json: bool) -> Result<()> {
    let r: Option<Resolved> = mesh_genesis::resolve(dir, licence).map_err(anyhow::Error::msg)?;
    match r {
        Some(r) => {
            if json {
                let conflict = r.conflicting_record.map(|c| c.to_hex());
                let v = serde_json::json!({ "mesh_id": r.id.to_hex(), "source": r.source.describe(), "conflicting_record": conflict });
                writeln!(w, "{}", serde_json::to_string_pretty(&v)?)?;
            } else {
                writeln!(w, "{}", r.id.to_hex())?;
                writeln!(w, "source: {}", r.source.describe())?;
                if let Some(c) = r.conflicting_record {
                    writeln!(w, "warning: the mesh record at {} names {}; the licence id is in use", mesh_genesis::record_path(dir).display(), c.to_hex())?;
                }
            }
            Ok(())
        }
        None => {
            if json {
                writeln!(w, "{}", serde_json::json!({ "mesh_id": null }))?;
            }
            bail!(
                "this node has no MeshId. On the mesh's primary run `weaver mesh create` (no Seed licence), or \
                 configure kernel.mesh.genesis_hash and mesh_nonce (Seed licence); other nodes learn it when they pair"
            )
        }
    }
}

#[cfg(test)]
#[path = "mesh_id_cmd_tests.rs"]
mod tests;
