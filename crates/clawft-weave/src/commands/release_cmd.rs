//! `weaver store release publish` (ADR-126 S4): CI writes a release into its release store, once.
//!
//! ```text
//! weaver store release publish <weftos://mesh/projects/ULID/releases/CHANNEL/store> \
//!     --build-id B --source-rev REV --dir ./dist --signing-key ci.key.json --s3-env ci.env
//! ```
//!
//! This talks to the object store directly with the credential in `--s3-env` (`MESH_S3_ENDPOINT`,
//! `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`), not through the daemon: the CI principal's policy
//! is PutObject and GetObject without DeleteObject (ADR-126 section 3), and the release API has no
//! overwrite. A rebuild of an existing build id exits non-zero. The same bytes again are a no-op.
//!
//! The signing key file is JSON `{"ed25519_seed": "<64 hex>", "ml_dsa65_seed": "<64 hex>"}` and must
//! not be readable by group or other.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Subcommand};
use weftos_release::{
    Artifact, PublishOutcome, PublishRequest, ReleaseBackend, ReleasePublisher, ReleaseSigner, ReleaseStoreName,
    s3::StoreBackend,
};

/// Largest single artifact: it is read into memory (a streaming upload comes with multipart support).
pub const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// `weaver store release ...`
#[derive(Args)]
pub struct ReleaseArgs {
    #[command(subcommand)]
    pub action: ReleaseAction,
}

#[derive(Subcommand)]
pub enum ReleaseAction {
    /// Publish every file under a directory as one release.
    Publish {
        /// `weftos://<mesh>/projects/<ULID>/releases/<channel>/store`
        store: String,
        #[arg(long)]
        build_id: String,
        /// Source revision the artifacts were built from.
        #[arg(long)]
        source_rev: String,
        /// Directory whose files become the artifacts (paths are relative to it).
        #[arg(long)]
        dir: PathBuf,
        /// Who built it (default: `ci`).
        #[arg(long, default_value = "ci")]
        builder: String,
        /// JSON file with the two signing seeds.
        #[arg(long)]
        signing_key: PathBuf,
        /// Env file with the CI credential for the object store.
        #[arg(long)]
        s3_env: PathBuf,
        /// Print the receipt as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Every regular file under `dir`, as artifacts with `/`-separated relative paths, sorted.
/// Symlinks, special files and files over [`MAX_FILE_BYTES`] are refused.
pub fn collect_artifacts(dir: &Path) -> anyhow::Result<Vec<Artifact>> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<Artifact>) -> anyhow::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(at)?.collect::<Result<_, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let ft = e.file_type()?;
            if ft.is_symlink() {
                anyhow::bail!("{}: symlinks are not published", path.display());
            }
            if ft.is_dir() {
                walk(root, &path, out)?;
            } else if ft.is_file() {
                let len = e.metadata()?.len();
                if len > MAX_FILE_BYTES {
                    anyhow::bail!("{}: {len} bytes is over the {MAX_FILE_BYTES}-byte limit", path.display());
                }
                let rel = path.strip_prefix(root)?;
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_str().ok_or_else(|| anyhow::anyhow!("{}: not UTF-8", path.display())))
                    .collect::<Result<Vec<_>, _>>()?
                    .join("/");
                out.push(Artifact { path: rel, bytes: std::fs::read(&path)? });
            } else {
                anyhow::bail!("{}: not a regular file", path.display());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    if out.is_empty() {
        anyhow::bail!("{}: no files to publish", dir.display());
    }
    Ok(out)
}

/// Read the signing key file. Refuses a file group or others can read.
pub fn load_signer(path: &Path) -> anyhow::Result<ReleaseSigner> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode();
        if mode & 0o077 != 0 {
            anyhow::bail!("{}: mode {:o} lets others read the signing key; chmod 600", path.display(), mode & 0o777);
        }
    }
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let seed = |k: &str| -> anyhow::Result<[u8; 32]> {
        let h = v[k].as_str().ok_or_else(|| anyhow::anyhow!("{}: {k} missing", path.display()))?;
        hex::decode(h)?.try_into().map_err(|_| anyhow::anyhow!("{}: {k} must be 32 bytes of hex", path.display()))
    };
    Ok(ReleaseSigner::from_seeds(&seed("ed25519_seed")?, &seed("ml_dsa65_seed")?))
}

/// The publish itself, over any backend.
pub async fn publish_dir<B: ReleaseBackend>(
    backend: &B,
    store: &ReleaseStoreName,
    signer: &ReleaseSigner,
    req: PublishRequest,
    now: u64,
) -> anyhow::Result<weftos_release::PublishReceipt> {
    Ok(ReleasePublisher::new(backend, store, signer).publish(req, now).await?)
}

fn parse_store(s: &str) -> anyhow::Result<ReleaseStoreName> {
    // weftos://<mesh>/projects/<ULID>/releases/<channel>/store
    let rest = s.strip_prefix("weftos://").ok_or_else(|| anyhow::anyhow!("{s:?}: not a weftos:// name"))?;
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [mesh, "projects", project, "releases", channel, "store"] => Ok(ReleaseStoreName::new(mesh, project, channel)?),
        _ => anyhow::bail!("{s:?}: expected weftos://<mesh>/projects/<ULID>/releases/<channel>/store"),
    }
}

/// Run a release subcommand.
pub async fn run(args: ReleaseArgs) -> anyhow::Result<()> {
    match args.action {
        ReleaseAction::Publish { store, build_id, source_rev, dir, builder, signing_key, s3_env, json } => {
            let name = parse_store(&store)?;
            let signer = load_signer(&signing_key)?;
            let artifacts = collect_artifacts(&dir)?;
            // The backend wants a secrets dir for admin key issuance, which a publish never uses.
            let scratch = tempfile::tempdir()?;
            let cfg = weftos_store::RustfsConfig::from_env_file(&s3_env, scratch.path().join("unused"))?;
            // Its HTTP client is blocking, so build it (and, via StoreBackend, drop it) off the async threads.
            let objects = Arc::new(tokio::task::spawn_blocking(move || weftos_store::RustfsBackend::new(cfg)).await??);
            let backend = StoreBackend::for_release(objects, &name)?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
            let req = PublishRequest { build_id, source_rev, created_at: now, builder, artifacts };
            let r = publish_dir(&backend, &name, &signer, req, now).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "outcome": match r.outcome { PublishOutcome::Published => "published", PublishOutcome::AlreadyPresent => "already-present" },
                        "build_id": r.build_id,
                        "manifest_digest": r.manifest_digest,
                        "artifacts": r.artifacts.iter().map(|a| serde_json::json!({"path": a.path, "hash": a.hash, "size": a.size})).collect::<Vec<_>>(),
                    })
                );
            } else {
                let verb = if r.outcome == PublishOutcome::Published { "published" } else { "already published, identical" };
                println!("{verb} {} ({} artifact(s), manifest {})", r.build_id, r.artifacts.len(), r.manifest_digest);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_release::{MemoryBackend, ReleaseError};
    use std::sync::Arc;

    const ULID: &str = "01JBXZ3N5C8QW4M2R7T9V6YH0K";

    fn req(arts: Vec<Artifact>, id: &str) -> PublishRequest {
        PublishRequest { build_id: id.into(), source_rev: "r".into(), created_at: 1_000, builder: "ci".into(), artifacts: arts }
    }

    #[test]
    fn store_names_parse_and_bad_ones_are_refused() {
        assert!(parse_store(&format!("weftos://m/projects/{ULID}/releases/stable/store")).is_ok());
        for bad in ["m/projects/x", "weftos://m/nodes/a/store", &format!("weftos://m/projects/{ULID}/store"), "weftos://m/projects/NOTULID/releases/s/store"] {
            assert!(parse_store(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn directory_becomes_sorted_slash_paths_and_refuses_symlinks_and_empty() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("bin")).unwrap();
        std::fs::write(d.path().join("bin/weaver"), b"w").unwrap();
        std::fs::write(d.path().join("SHA256SUMS"), b"s").unwrap();
        let a = collect_artifacts(d.path()).unwrap();
        assert_eq!(a.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(), vec!["SHA256SUMS", "bin/weaver"]);

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", d.path().join("link")).unwrap();
            assert!(collect_artifacts(d.path()).err().unwrap().to_string().contains("symlink"));
        }
        let empty = tempfile::tempdir().unwrap();
        assert!(collect_artifacts(empty.path()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn signing_key_must_be_private_and_well_formed() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("k.json");
        let body = serde_json::json!({"ed25519_seed": "07".repeat(32), "ml_dsa65_seed": "09".repeat(32)});
        std::fs::write(&p, body.to_string()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_signer(&p).err().unwrap().to_string().contains("chmod 600"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let s = load_signer(&p).unwrap();
        assert_eq!(s.key_id().len(), 32);
        std::fs::write(&p, r#"{"ed25519_seed": "zz", "ml_dsa65_seed": "00"}"#).unwrap();
        assert!(load_signer(&p).is_err());
    }

    #[tokio::test]
    async fn publish_once_retry_is_noop_rebuild_is_refused() {
        let st = ReleaseStoreName::new("m", ULID, "stable").unwrap();
        let signer = ReleaseSigner::from_seeds(&[7; 32], &[9; 32]);
        let be = MemoryBackend::new();
        let a = |b: &[u8]| vec![Artifact { path: "bin/weaver".into(), bytes: b.to_vec() }];
        let r = publish_dir(&be, &st, &signer, req(a(b"v1"), "b1"), 1_000).await.unwrap();
        assert_eq!(r.outcome, PublishOutcome::Published);
        let r = publish_dir(&be, &st, &signer, req(a(b"v1"), "b1"), 1_000).await.unwrap();
        assert_eq!(r.outcome, PublishOutcome::AlreadyPresent);
        let e = publish_dir(&be, &st, &signer, req(a(b"EVIL"), "b1"), 1_000).await.unwrap_err();
        assert!(matches!(e.downcast_ref::<ReleaseError>(), Some(ReleaseError::AlreadyPublished { .. })), "{e}");
    }

    /// Against a THROWAWAY RustFS: `WEFTOS_RELEASE_TEST_RUSTFS_ENV=<env file> cargo test -p clawft-weave
    /// live_publish -- --ignored`. Creates its own bucket; never point it at PG.
    #[tokio::test]
    #[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
    async fn live_publish_through_the_verb() {
        use std::os::unix::fs::PermissionsExt;
        let env = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV").expect("WEFTOS_RELEASE_TEST_RUSTFS_ENV");
        let d = tempfile::tempdir().unwrap();
        let dist = d.path().join("dist");
        std::fs::create_dir_all(dist.join("bin")).unwrap();
        std::fs::write(dist.join("bin/weaver"), vec![5u8; 1 << 20]).unwrap();
        std::fs::write(dist.join("SHA256SUMS"), b"sums").unwrap();
        let key = d.path().join("k.json");
        std::fs::write(&key, serde_json::json!({"ed25519_seed": "07".repeat(32), "ml_dsa65_seed": "09".repeat(32)}).to_string()).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();

        let channel = format!("t{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
        let name = format!("weftos://m/projects/{ULID}/releases/{channel}/store");
        // make the bucket as the root credential
        let st = parse_store(&name).unwrap();
        let (envp, root_dir) = (std::path::PathBuf::from(&env), d.path().join("root"));
        let bucket = StoreBackend::<weftos_store::RustfsBackend>::for_release(
            Arc::new(tokio::task::spawn_blocking(move || {
                weftos_store::RustfsBackend::new(weftos_store::RustfsConfig::from_env_file(&envp, root_dir).unwrap())
            }).await.unwrap().unwrap()),
            &st,
        ).unwrap();
        let b = bucket.bucket().to_string();
        let envp = std::path::PathBuf::from(&env);
        let root_dir = d.path().join("root2");
        tokio::task::spawn_blocking(move || {
            use weftos_store::AdminBackend;
            weftos_store::RustfsBackend::new(weftos_store::RustfsConfig::from_env_file(&envp, root_dir).unwrap())
                .unwrap()
                .create_bucket(&b, &[])
        }).await.unwrap().unwrap();
        drop(bucket);

        let go = |id: &str, bytes_dir: &Path| ReleaseArgs {
            action: ReleaseAction::Publish {
                store: name.clone(), build_id: id.into(), source_rev: "r".into(), dir: bytes_dir.to_path_buf(),
                builder: "ci".into(), signing_key: key.clone(), s3_env: PathBuf::from(&env), json: true,
            },
        };
        run(go("b1", &dist)).await.unwrap();
        run(go("b1", &dist)).await.unwrap(); // identical retry
        std::fs::write(dist.join("SHA256SUMS"), b"REBUILT").unwrap();
        let e = run(go("b1", &dist)).await.expect_err("a rebuild must be refused");
        assert!(e.to_string().contains("already"), "{e}");
    }
}
