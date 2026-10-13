//! `weaver mesh create | adopt-licence | id`: the mesh's `MeshId` (ADR-114 §2).
//! The work is in [`crate::mesh_genesis`]; this file reads the keys and the
//! config, calls it and prints. Nothing here prints key material.

use std::io::{Read, Write};
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

/// `weaver mesh export`.
#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Write the record here (mode 0600, never replaces a file); default stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// `weaver mesh import`.
#[derive(Args, Debug)]
pub struct ImportArgs {
    /// The exported record, or `-` for stdin.
    pub file: String,
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

/// `weaver mesh export`.
pub async fn export(w: &mut dyn Write, a: &ExportArgs) -> Result<()> {
    let (dir, _) = local_state().await?;
    export_with(w, &dir, a.out.as_deref())
}

/// `export` over explicit inputs. Exports the stored record only (a licence
/// id with no record has nothing to export).
pub fn export_with(w: &mut dyn Write, dir: &Path, out: Option<&Path>) -> Result<()> {
    let stored = mesh_genesis::load(dir)
        .map_err(anyhow::Error::msg)?
        .ok_or_else(|| anyhow!("this node holds no mesh record to export (`weaver mesh id` shows where its id comes from)"))?;
    let text = serde_json::to_string_pretty(&stored.signed)?;
    match out {
        None => writeln!(w, "{text}")?,
        Some(p) => {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(p)
                .with_context(|| format!("{}", p.display()))?;
            writeln!(f, "{text}")?;
            writeln!(w, "mesh record for {} written to {}", stored.signed.record.mesh_id, p.display())?;
        }
    }
    Ok(())
}

/// `weaver mesh import`.
pub async fn import(w: &mut dyn Write, a: &ImportArgs) -> Result<()> {
    let (dir, licence) = local_state().await?;
    let text = if a.file == "-" {
        let mut t = String::new();
        std::io::Read::take(std::io::stdin(), 16 * 1024 + 1).read_to_string(&mut t).context("reading stdin")?;
        t
    } else {
        std::fs::read_to_string(&a.file).with_context(|| a.file.clone())?
    };
    import_with(w, &dir, licence, &text)
}

/// `import` over explicit inputs.
pub fn import_with(w: &mut dyn Write, dir: &Path, licence: Option<MeshId>, text: &str) -> Result<()> {
    use crate::mesh_import::{Imported, import, parse};
    let signed = parse(text).map_err(anyhow::Error::msg)?;
    match import(dir, &signed, licence).map_err(anyhow::Error::msg)? {
        Imported::Written(id) => {
            writeln!(w, "mesh id: {}", id.to_hex())?;
            writeln!(w, "record:  {}", mesh_genesis::record_path(dir).display())?;
            writeln!(w, "imported from node {}; restart the daemon so it uses it", clawft_kernel::node_id_from_pubkey(&hex::decode(&signed.signer)?.try_into().map_err(|_| anyhow!("signer"))?))?;
        }
        Imported::AlreadyHeld(id) => writeln!(w, "mesh id: {} (this node already holds this record; nothing to do)", id.to_hex())?,
    }
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
