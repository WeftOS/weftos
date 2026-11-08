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
    Artifact, LockMode, LockPolicy, PublishOutcome, PublishRequest, ReleaseBackend, ReleasePublisher, ReleaseSigner, ReleaseStoreName,
    route::CiRoute, s3::StoreBackend,
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
        /// Object Lock mode for what this publish writes: `governance` or `compliance`. The bucket must
        /// have been made with Object Lock. `compliance` cannot be undone by anyone, owner included.
        #[arg(long, value_parser = ["governance", "compliance"], requires = "lock_days")]
        lock: Option<String>,
        /// Days to keep the manifest and new blobs, counted from now (1 to 3650).
        #[arg(long, requires = "lock")]
        lock_days: Option<u64>,
        /// How CI reaches the bucket: `direct` (this key talks to the object store) or `daemon` (through the
        /// weaver daemon's store service as an `append` principal). Default: `weftos_release::route::DEFAULT_CI_ROUTE`.
        #[arg(long, value_parser = ["direct", "daemon"])]
        ci_route: Option<String>,
        /// Print the receipt as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// The route the flag names, or the library default.
pub fn resolve_route(flag: Option<&str>) -> anyhow::Result<CiRoute> {
    let route = match flag {
        Some(s) => CiRoute::parse(s).map_err(|e| anyhow::anyhow!("{e}"))?,
        None => CiRoute::default(),
    };
    if route == CiRoute::Daemon {
        anyhow::bail!(
            "CI route is daemon: that route runs inside the weaver daemon (svc.store, `append` grant), and this \
             command has no daemon client yet. Use --ci-route direct with a CI key whose policy is \
             weftos_release::s3::direct_ci_policy, or publish from the daemon."
        );
    }
    Ok(route)
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
    lock: Option<LockPolicy>,
) -> anyhow::Result<weftos_release::PublishReceipt> {
    let mut p = ReleasePublisher::new(backend, store, signer);
    if let Some(l) = lock {
        p = p.with_lock(l);
    }
    Ok(p.publish(req, now).await?)
}

/// A refused write is almost always the credential, not the release: say which roles can write.
pub fn explain_denied(e: anyhow::Error) -> anyhow::Error {
    let m = e.to_string();
    let low = m.to_lowercase();
    if low.contains("forbidden") || low.contains("accessdenied") || low.contains("access denied") {
        anyhow::anyhow!(
            "{m}\nhint: this credential may not write. S1's CI `append` role is read-only at RustFS (its puts go \
             through the daemon). A key that writes straight to the bucket needs s3:PutObject, and with --lock also \
             s3:PutObjectRetention (see weftos_release::s3::direct_ci_policy)."
        )
    } else {
        e
    }
}

/// `--lock` and `--lock-days` as a policy.
pub fn lock_policy(mode: Option<&str>, days: Option<u64>) -> anyhow::Result<Option<LockPolicy>> {
    let (Some(m), Some(d)) = (mode, days) else { return Ok(None) };
    let mode = match m {
        "governance" => LockMode::Governance,
        "compliance" => LockMode::Compliance,
        other => anyhow::bail!("unknown lock mode {other:?}"),
    };
    if !(1..=3650).contains(&d) {
        anyhow::bail!("--lock-days must be 1 to 3650");
    }
    Ok(Some(LockPolicy::new(mode, d * 86_400)?))
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
        ReleaseAction::Publish { store, build_id, source_rev, dir, builder, signing_key, s3_env, lock, lock_days, ci_route, json } => {
            let _route = resolve_route(ci_route.as_deref())?;
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
            let r = publish_dir(&backend, &name, &signer, req, now, lock_policy(lock.as_deref(), lock_days)?)
                .await
                .map_err(explain_denied)?;
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
    use clap::Parser;
    use weftos_release::{MemoryBackend, ReleaseError};
    use std::sync::Arc;

    const ULID: &str = "01JBXZ3N5C8QW4M2R7T9V6YH0K";

    fn req(arts: Vec<Artifact>, id: &str) -> PublishRequest {
        PublishRequest { build_id: id.into(), source_rev: "r".into(), created_at: 1_000, builder: "ci".into(), artifacts: arts }
    }

    #[test]
    fn a_refused_write_explains_which_roles_can_write() {
        let denied = anyhow::anyhow!("store backend: forbidden: AccessDenied Access Denied");
        let m = explain_denied(denied).to_string();
        assert!(m.contains("PutObjectRetention") && m.contains("append"), "{m}");
        let other = explain_denied(anyhow::anyhow!("build b1 is already published")).to_string();
        assert!(!other.contains("hint"), "{other}");
    }

    #[test]
    fn the_ci_route_flag_picks_a_route_and_the_daemon_route_is_refused_here() {
        assert_eq!(resolve_route(Some("direct")).unwrap(), CiRoute::Direct);
        assert_eq!(resolve_route(None).unwrap(), weftos_release::route::DEFAULT_CI_ROUTE);
        let e = resolve_route(Some("daemon")).unwrap_err().to_string();
        assert!(e.contains("no daemon client") && e.contains("--ci-route direct"), "{e}");
        assert!(resolve_route(Some("both")).is_err());
        let args = |a: &[&str]| crate::commands::store_cmd::StoreArgs::try_parse_from(std::iter::once("store").chain(a.iter().copied()));
        let base = ["release", "publish", "weftos://m/projects/x/releases/c/store", "--build-id", "b", "--source-rev", "r", "--dir", "d", "--signing-key", "k", "--s3-env", "e"];
        assert!(args(&[&base[..], &["--ci-route", "direct"]].concat()).is_ok());
        assert!(args(&[&base[..], &["--ci-route", "daemon"]].concat()).is_ok(), "parses; refused when run");
        assert!(args(&[&base[..], &["--ci-route", "x"]].concat()).is_err());
    }

    #[test]
    fn lock_flags_become_a_policy_or_an_error() {
        assert!(lock_policy(None, None).unwrap().is_none());
        let p = lock_policy(Some("compliance"), Some(30)).unwrap().unwrap();
        assert_eq!((p.mode, p.floor_secs), (LockMode::Compliance, 30 * 86_400));
        assert!(lock_policy(Some("governance"), Some(0)).is_err());
        assert!(lock_policy(Some("governance"), Some(3651)).is_err());
        assert!(lock_policy(Some("legal"), Some(1)).is_err());
        let args = |a: &[&str]| crate::commands::store_cmd::StoreArgs::try_parse_from(std::iter::once("store").chain(a.iter().copied()));
        let base = ["release", "publish", "weftos://m/projects/x/releases/c/store", "--build-id", "b", "--source-rev", "r", "--dir", "d", "--signing-key", "k", "--s3-env", "e"];
        assert!(args(&base).is_ok());
        assert!(args(&[&base[..], &["--lock", "compliance", "--lock-days", "30"]].concat()).is_ok());
        assert!(args(&[&base[..], &["--lock", "compliance"]].concat()).is_err(), "needs --lock-days");
        assert!(args(&[&base[..], &["--lock-days", "3"]].concat()).is_err(), "needs --lock");
        assert!(args(&[&base[..], &["--lock", "nope", "--lock-days", "3"]].concat()).is_err());
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
        let r = publish_dir(&be, &st, &signer, req(a(b"v1"), "b1"), 1_000, None).await.unwrap();
        assert_eq!(r.outcome, PublishOutcome::Published);
        let r = publish_dir(&be, &st, &signer, req(a(b"v1"), "b1"), 1_000, None).await.unwrap();
        assert_eq!(r.outcome, PublishOutcome::AlreadyPresent);
        let e = publish_dir(&be, &st, &signer, req(a(b"EVIL"), "b1"), 1_000, None).await.unwrap_err();
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
                builder: "ci".into(), signing_key: key.clone(), s3_env: PathBuf::from(&env), lock: None, lock_days: None, ci_route: None, json: true,
            },
        };
        run(go("b1", &dist)).await.unwrap();
        run(go("b1", &dist)).await.unwrap(); // identical retry
        std::fs::write(dist.join("SHA256SUMS"), b"REBUILT").unwrap();
        let e = run(go("b1", &dist)).await.expect_err("a rebuild must be refused");
        assert!(e.to_string().contains("already"), "{e}");

        // a bucket without Object Lock refuses lock headers, and the publish writes no release
        let mut with_lock = go("b2", &dist);
        let ReleaseAction::Publish { lock, lock_days, .. } = &mut with_lock.action;
        (*lock, *lock_days) = (Some("governance".into()), Some(1));
        assert!(run(with_lock).await.is_err(), "lock on a plain bucket must fail");

        // a locked bucket takes a locked publish
        let lchan = format!("l{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
        let lname = format!("weftos://m/projects/{ULID}/releases/{lchan}/store");
        let (lst, envp, ldir) = (parse_store(&lname).unwrap(), std::path::PathBuf::from(&env), d.path().join("root3"));
        tokio::task::spawn_blocking(move || {
            let be = weftos_store::RustfsBackend::new(weftos_store::RustfsConfig::from_env_file(&envp, ldir).unwrap()).unwrap();
            weftos_release::s3::create_locked_release_bucket(&be, &lst)
        }).await.unwrap().unwrap();
        let locked = ReleaseArgs {
            action: ReleaseAction::Publish {
                store: lname, build_id: "b1".into(), source_rev: "r".into(), dir: dist.clone(),
                builder: "ci".into(), signing_key: key.clone(), s3_env: PathBuf::from(&env),
                lock: Some("compliance".into()), lock_days: Some(2), ci_route: Some("direct".into()), json: true,
            },
        };
        run(locked).await.unwrap();
    }

    /// The verb with NON-ROOT keys on a locked bucket: the direct CI role publishes locked; S1's
    /// read-only append role is refused with a hint. Throwaway RustFS only (see above).
    #[tokio::test]
    #[ignore = "needs a throwaway RustFS: WEFTOS_RELEASE_TEST_RUSTFS_ENV"]
    async fn live_verb_with_non_root_ci_keys() {
        use std::os::unix::fs::PermissionsExt;
        use weftos_store::{AdminBackend, ObjectBackend};
        let env = std::env::var("WEFTOS_RELEASE_TEST_RUSTFS_ENV").expect("WEFTOS_RELEASE_TEST_RUSTFS_ENV");
        let d = tempfile::tempdir().unwrap();
        let dist = d.path().join("dist");
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::write(dist.join("weaver"), b"bin").unwrap();
        let key = d.path().join("k.json");
        std::fs::write(&key, serde_json::json!({"ed25519_seed": "07".repeat(32), "ml_dsa65_seed": "09".repeat(32)}).to_string()).unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        let channel = format!("n{}", &ulid::Ulid::new().to_string().to_lowercase()[16..]);
        let name = format!("weftos://m/projects/{ULID}/releases/{channel}/store");
        let st = parse_store(&name).unwrap();

        // root makes the locked bucket and two keys; write an env file per key
        let (envp, root_dir, kdir) = (PathBuf::from(&env), d.path().join("root"), d.path().to_path_buf());
        let sfx = ulid::Ulid::new().to_string().to_lowercase()[16..].to_owned();
        let (direct_env, append_env) = tokio::task::spawn_blocking(move || {
            let cfg = weftos_store::RustfsConfig::from_env_file(&envp, root_dir).unwrap();
            let root = weftos_store::RustfsBackend::new(cfg.clone()).unwrap();
            let bucket = weftos_release::s3::create_locked_release_bucket(&root, &st).unwrap();
            let mk = |tag: &str, policy: serde_json::Value| -> PathBuf {
                let id = root.create_access_key(&format!("cog:ci-{tag}-{sfx}")).unwrap().id;
                root.put_policy(&id, &policy).unwrap();
                let secret = std::fs::read_to_string(root.secret_path(&id)).unwrap();
                let f = kdir.join(format!("{tag}.env"));
                std::fs::write(&f, format!("MESH_S3_ENDPOINT={}\nMESH_S3_ACCESS_KEY={id}\nMESH_S3_SECRET_KEY={secret}\n", cfg.endpoint)).unwrap();
                f
            };
            let append = serde_json::json!({"Version": "2012-10-17", "Statement": [{"Effect": "Allow",
                "Action": ["s3:GetObject", "s3:ListBucket", "s3:GetBucketLocation"],
                "Resource": [format!("arn:aws:s3:::{bucket}"), format!("arn:aws:s3:::{bucket}/*")]}]});
            let _ = root.head(&bucket, "none"); // warm the connection; result irrelevant
            (mk("direct", weftos_release::s3::direct_ci_policy(&bucket)), mk("append", append))
        }).await.unwrap();

        let go = |id: &str, s3: &Path| ReleaseArgs {
            action: ReleaseAction::Publish {
                store: name.clone(), build_id: id.into(), source_rev: "r".into(), dir: dist.clone(),
                builder: "ci".into(), signing_key: key.clone(), s3_env: s3.to_path_buf(),
                lock: Some("compliance".into()), lock_days: Some(2), ci_route: Some("direct".into()), json: true,
            },
        };
        run(go("b1", &direct_env)).await.unwrap();
        run(go("b1", &direct_env)).await.unwrap(); // identical retry from the same key
        let e = run(go("b2", &append_env)).await.expect_err("the append role cannot write at RustFS");
        assert!(e.to_string().contains("PutObjectRetention"), "{e}");
    }
}
