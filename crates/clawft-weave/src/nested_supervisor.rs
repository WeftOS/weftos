//! D10 user-instance lifecycle. One owned process handle per instance; never
//! adopt/signal an unverified PID and never attach to the machine service.
use nix::libc;
use crate::nested_boot::{BootContract, CONTRACT_ENV, MASTER_ENV, config_hash, constrain_config};
use anyhow::{Context, ensure};
use clawft_kernel::governance_overlay::{Overlay, merge};
use clawft_kernel::parent_policy::{ParentPolicy, export_rules, write_atomic_0600};
use clawft_types::config::{
    Config,
    nested::{NestedInstance, NestedRegistration},
    overlay::OverlayFile,
};
use clawft_types::project::canon::hex_encode;
use clawft_types::project::cert::key_id;
use ed25519_dalek::SigningKey;
use rand::{RngCore, rngs::OsRng};
use serde_json::json;
use std::collections::BTreeMap;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

static GLOBAL: OnceLock<Arc<NestedSupervisor>> = OnceLock::new();
pub fn global() -> Option<Arc<NestedSupervisor>> {
    GLOBAL.get().cloned()
}
pub fn install(sup: Arc<NestedSupervisor>) -> anyhow::Result<()> {
    GLOBAL
        .set(sup)
        .map_err(|_| anyhow::anyhow!("nested supervisor already installed"))
}

struct Instance {
    contract: BootContract,
    config: Config,
    cap: OverlayFile,
    child: Option<Child>,
    revoked: bool,
}

pub struct NestedSupervisor {
    root: PathBuf,
    exe: PathBuf,
    key: SigningKey,
    depth: u32,
    instances: Mutex<BTreeMap<String, Instance>>,
}

impl NestedSupervisor {
    /// The daemon calls this only when its effective config has weave.master.
    pub fn new(
        root: PathBuf,
        exe: PathBuf,
        key: SigningKey,
        master: bool,
        depth: u32,
    ) -> anyhow::Result<Self> {
        ensure!(master, "nested lifecycle requires weave.master = true");
        ensure!(depth < 32, "maximum nesting depth reached");
        private_dir(&root)?;
        let root = root.canonicalize()?;
        let mut instances = BTreeMap::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            ensure!(
                entry.file_type()?.is_dir(),
                "unexpected nested registry entry"
            );
            let contract: BootContract =
                serde_json::from_slice(&std::fs::read(entry.path().join("boot.json"))?)?;
            // Registry recovery verifies signature and shape; boot freshness is
            // checked separately in the child after issuing a NEW generation.
            contract.verify(&key.verifying_key().to_bytes(), i64::MIN)?;
            ensure!(
                entry.path() == root.join(&config_hash(contract.instance.id.as_bytes())[..16])
                    && contract.home == entry.path().join("h")
                    && contract.config == entry.path().join("config.json"),
                "nested registry paths changed"
            );
            let bytes = std::fs::read(&contract.config)?;
            ensure!(
                config_hash(&bytes) == contract.config_hash,
                "nested registry config changed"
            );
            let config = serde_json::from_slice(&bytes)?;
            let revoked = entry.path().join("boot.revoked").exists();
            let cap = contract.cap.clone();
            let id = contract.instance.id.clone();
            instances.insert(
                id,
                Instance {
                    contract,
                    config,
                    cap,
                    child: None,
                    revoked,
                },
            );
        }
        Ok(Self {
            root,
            exe: exe.canonicalize()?,
            key,
            depth,
            instances: Mutex::new(instances),
        })
    }

    pub fn instance_dir(&self, id: &str) -> PathBuf {
        // Short stable path leaves room for a supervised project's ULID on macOS.
        self.root.join(&config_hash(id.as_bytes())[..16])
    }

    /// A cap must tighten the live master's signed policy. It is not an
    /// alternative policy engine, and a looser requested cap is an error.
    fn capped(&self, parent: &ParentPolicy, cap: &OverlayFile) -> anyhow::Result<ParentPolicy> {
        clawft_kernel::parent_policy::verify_parent_policy(
            parent,
            &self.key.verifying_key().to_bytes(),
        )?;
        let effective = merge(parent, &Overlay::from_file(cap.clone()))?;
        Ok(export_rules(
            effective.rules.clone(),
            effective.risk_threshold(0.7),
            effective.human_approval(false),
            &effective.limits,
            &self.key,
            parent.version,
            chrono::Utc::now(),
        )?)
    }

    pub async fn register(
        &self,
        id: &str,
        mut config: Config,
        parent: ParentPolicy,
        cap: OverlayFile,
    ) -> anyhow::Result<String> {
        clawft_types::project::validate_id(id)?;
        let policy = self.capped(&parent, &cap)?;
        let mut all = self.instances.lock().await;
        ensure!(!all.contains_key(id), "nested instance already registered");
        let dir = self.instance_dir(id);
        // Never reuse another lane's state, a stale registration, or a symlink.
        std::fs::create_dir(&dir)
            .context("nested registration directory already exists or is inaccessible")?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let home = dir.join("h");
        let runtime = home.join("r");
        for p in [
            &home,
            &home.join(".weftos"),
            &runtime,
            &home.join(".weftos/chain"),
            &home.join(".weftos/projects"),
            &home.join(".weftos/state"),
            &home.join(".config"),
            &home.join("tmp"),
        ] {
            private_dir(p)?;
        }
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        let inner = SigningKey::from_bytes(&seed);
        write_atomic_0600(&home.join(".weftos/user.key"), &seed)?;
        write_atomic_0600(&runtime.join("node.key"), &seed)?;
        let mut contract = BootContract {
            schema: 1,
            generation: 0,
            cap: cap.clone(),
            instance: NestedInstance {
                id: id.into(),
                parent: key_id(&self.key.verifying_key().to_bytes()),
                depth: self.depth + 1,
            },
            home,
            runtime,
            config: dir.join("config.json"),
            config_hash: String::new(),
            inner_pubkey: hex_encode(&inner.verifying_key().to_bytes()),
            registration: NestedRegistration::Isolated,
            policy,
            expires_at: 0,
            sig: String::new(),
        };
        constrain_config(&mut config, &contract)?;
        let bytes = serde_json::to_vec(&config)?;
        contract.config_hash = config_hash(&bytes);
        let inner_id = key_id(&inner.verifying_key().to_bytes());
        let mut instance = Instance {
            contract,
            config,
            cap,
            child: None,
            revoked: false,
        };
        self.persist(&mut instance)?;
        all.insert(id.into(), instance);
        Ok(inner_id)
    }

    fn persist(&self, instance: &mut Instance) -> anyhow::Result<()> {
        let c = &mut instance.contract;
        constrain_config(&mut instance.config, c)?;
        let config = serde_json::to_vec(&instance.config)?;
        c.config_hash = config_hash(&config);
        c.generation = c
            .generation
            .checked_add(1)
            .context("nested generation exhausted")?;
        c.expires_at = chrono::Utc::now().timestamp() + 60;
        c.sign(&self.key);
        write_atomic_0600(&c.config, &config)?;
        clawft_kernel::overlay_runtime::write_user_pin(
            &c.runtime,
            &self.key.verifying_key().to_bytes(),
        )?;
        write_atomic_0600(
            &c.runtime.join("parent-policy.json"),
            &serde_json::to_vec(&c.policy)?,
        )?;
        write_atomic_0600(
            &c.config.with_file_name("boot.json"),
            &serde_json::to_vec(c)?,
        )?;
        Ok(())
    }

    pub fn command(&self, contract: &BootContract) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.env_clear()
            .current_dir(&contract.home)
            .args([
                "kernel",
                "start",
                "--foreground",
                "--profile",
                "user",
                "--config",
            ])
            .arg(&contract.config)
            .env("HOME", &contract.home)
            .env("XDG_CONFIG_HOME", contract.home.join(".config"))
            .env("XDG_DATA_HOME", contract.home.join(".local/share"))
            .env("XDG_CACHE_HOME", contract.home.join(".cache"))
            .env("TMPDIR", contract.home.join("tmp"))
            .env("WEFTOS_RUNTIME_DIR", &contract.runtime)
            .env(CONTRACT_ENV, contract.config.with_file_name("boot.json"))
            .env(MASTER_ENV, hex_encode(&self.key.verifying_key().to_bytes()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd.as_std_mut().process_group(0);
        cmd
    }

    pub async fn start(&self, id: &str) -> anyhow::Result<serde_json::Value> {
        let mut all = self.instances.lock().await;
        let instance = all.get_mut(id).context("nested instance not registered")?;
        ensure!(!instance.revoked, "nested instance revoked");
        if let Some(child) = &mut instance.child
            && child.try_wait()?.is_none()
        {
        return Ok(json!({"id": id, "pid": child.id(), "started": false}));
        }
        instance.child = None;
        // Recovery may have no Child handle even though the old daemon is still
        // alive. Prove it is gone before rewriting its boot/policy files.
        Self::terminate(instance).await?;
        self.persist(instance)?;
        ensure!(
            instance
                .contract
                .runtime
                .join("00000000000000000000000000/kernel.sock")
                .as_os_str()
                .len()
                < 104,
            "nested runtime is too long for supervised project sockets; use a shorter master runtime"
        );
        {
            let paths = clawft_types::runtime_paths::RuntimePaths::at(&instance.contract.runtime);
            let _lock = crate::instance_lock::InstanceLock::acquire(&paths)?;
            crate::instance_lock::reclaim_stale_socket(&paths).await?;
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(instance.contract.config.with_file_name("daemon.log"))?;
        let mut command = self.command(&instance.contract);
        command
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        let mut child = command.spawn()?;
        // Keep the process handle (and stdin parent-liveness pipe) before any await.
        let pid = child.id();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("nested user exited before readiness: {status}");
            }
            let probe = async {
                let mut client = clawft_rpc::DaemonClient::connect_path(
                    instance.contract.runtime.join("kernel.sock"),
                )
                .await?;
                let response = client.simple_call("kernel.handshake").await.ok()?;
                let h = response.result?;
                let inner = clawft_types::project::canon::hex_decode::<32>(
                    &instance.contract.inner_pubkey,
                )?;
                (response.ok
                    && h["profile"] == "user"
                    && h["project_id"].is_null()
                    && h["user_key_id"] == key_id(&inner)
                    && h["node_id"] == key_id(&inner)
                    && h["depth"] == instance.contract.instance.depth
                    && h["parent"] == instance.contract.instance.parent
                    && h["pid"].as_u64() == pid.map(u64::from))
                .then_some(())
            };
            if matches!(
                tokio::time::timeout(std::time::Duration::from_millis(250), probe).await,
                Ok(Some(()))
            ) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                child.kill().await?;
                anyhow::bail!("nested user readiness timed out");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        instance.child = Some(child);
        Ok(
            json!({"id": id, "pid": pid, "started": true, "home": instance.contract.home, "socket": instance.contract.runtime.join("kernel.sock")}),
        )
    }

    async fn terminate(instance: &mut Instance) -> anyhow::Result<()> {
        if let Some(child) = &mut instance.child
            && child.try_wait()?.is_none()
        {
                // Try RPC briefly, but shutdown policy (including deny_all)
                // must not prevent the owner's graceful project cascade.
                let graceful = async {
                    let request_shutdown = async {
                        let mut client = clawft_rpc::DaemonClient::connect_path(
                            instance.contract.runtime.join("kernel.sock"),
                        )
                        .await?;
                        let response = client.simple_call("kernel.shutdown").await.ok()?;
                        response.ok.then_some(())
                    };
                    let accepted = matches!(
                        tokio::time::timeout(std::time::Duration::from_secs(2), request_shutdown)
                            .await,
                        Ok(Some(()))
                    );
                    // `Child::wait` closes the child's stdin before it waits, and EOF on
                    // that pipe makes the child SIGTERM itself. After an accepted
                    // `kernel.shutdown` that second signal races the shutdown the RPC
                    // started: if it lands once the guard has begun, the daemon hard-exits
                    // ("SIGTERM during shutdown") before it saves its chain. So hold the
                    // pipe open until the child has exited; close it early only when the
                    // RPC was refused, which is the case the EOF cascade exists for.
                    let held = child.stdin.take();
                    if !accepted {
                        // This pipe belongs to our Child handle. EOF invokes
                        // the child's existing SIGTERM/cascade watcher; no PID
                        // lookup or signal to a recovered process is involved.
                        drop(held);
                        child.wait().await
                    } else {
                        // `held` is no longer in `child`, so `wait` has nothing to close.
                        let status = child.wait().await;
                        drop(held);
                        status
                    }
                };
                if !matches!(
                    tokio::time::timeout(std::time::Duration::from_secs(15), graceful).await,
                    Ok(Ok(_))
                ) {
                    child
                        .kill()
                        .await
                        .context("could not stop nested instance; revocation incomplete")?;
                }
        }
        instance.child = None;
        // No handle after registry recovery is NOT evidence of death. The
        // runtime lock and socket probe jointly prove quiescence. Never signal
        // a recovered PID, or unlink an unowned endpoint that accepts calls.
        let paths = clawft_types::runtime_paths::RuntimePaths::at(&instance.contract.runtime);
        let _lock = crate::instance_lock::InstanceLock::acquire(&paths)
            .context("nested instance may still be alive; stop/revocation incomplete")?;
        crate::instance_lock::reclaim_stale_socket(&paths)
            .await
            .context("nested endpoint still live or unverifiable; stop/revocation incomplete")?;
        Ok(())
    }

    pub async fn stop(&self, id: &str) -> anyhow::Result<()> {
        let mut all = self.instances.lock().await;
        Self::terminate(all.get_mut(id).context("nested instance not registered")?).await
    }

    /// Changing a grant closes every old session before changing the signed
    /// config. Explicit start is required after grant/revoke or policy changes.
    pub async fn grant(&self, id: &str, registration: NestedRegistration) -> anyhow::Result<()> {
        let mut all = self.instances.lock().await;
        let instance = all.get_mut(id).context("nested instance not registered")?;
        ensure!(!instance.revoked, "nested instance revoked");
        let inner = clawft_types::project::canon::hex_decode::<32>(&instance.contract.inner_pubkey)
            .context("inner key")?;
        registration
            .validate(&key_id(&inner))
            .map_err(anyhow::Error::msg)?;
        Self::terminate(instance).await?;
        instance.contract.registration = registration;
        self.persist(instance)
    }

    pub async fn revoke(&self, id: &str) -> anyhow::Result<()> {
        let mut all = self.instances.lock().await;
        let instance = all.get_mut(id).context("nested instance not registered")?;
        instance.revoked = true;
        write_atomic_0600(&self.instance_dir(id).join("boot.revoked"), b"revoked\n")?;
        write_atomic_0600(&instance.contract.runtime.join("revoked"), b"revoked\n")?;
        Self::terminate(instance).await
    }

    pub async fn refresh_policy(&self, id: &str, parent: ParentPolicy) -> anyhow::Result<()> {
        let mut all = self.instances.lock().await;
        let instance = all.get_mut(id).context("nested instance not registered")?;
        ensure!(
            parent.version >= instance.contract.policy.version,
            "policy rollback refused"
        );
        let policy = self.capped(&parent, &instance.cap)?;
        // Metadata (version/time/signature) changes on each export. Compare
        // the complete effective rules+limits, then keep the LIVE signed bytes
        // unchanged for a semantic no-op. The overlay runtime pins that exact
        // signature; rewriting it while running breaks reload availability.
        if same_effective_policy(&policy, &instance.contract.policy)? {
            return Ok(());
        }
        Self::terminate(instance).await?;
        instance.contract.policy = policy;
        self.persist(instance)
    }

    pub async fn push_policy(&self, parent: ParentPolicy) -> anyhow::Result<()> {
        let mut all = self.instances.lock().await;
        // Stop first: even an invalid/newly incompatible cap cannot leave an old
        // weaker policy or old mesh sessions running after master policy changes.
        for instance in all.values_mut() {
            Self::terminate(instance).await?;
        }
        for instance in all.values_mut().filter(|i| !i.revoked) {
            ensure!(
                parent.version > instance.contract.policy.version,
                "policy update must advance version"
            );
            instance.contract.policy = self.capped(&parent, &instance.cap)?;
            self.persist(instance)?;
        }
        Ok(())
    }

    pub async fn stop_all(&self) -> anyhow::Result<()> {
        let mut all = self.instances.lock().await;
        for instance in all.values_mut() {
            Self::terminate(instance).await?;
        }
        Ok(())
    }
}

fn private_dir(path: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    ensure!(
        std::fs::symlink_metadata(path)?.file_type().is_dir(),
        "nested directory is not a directory"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn same_effective_policy(a: &ParentPolicy, b: &ParentPolicy) -> anyhow::Result<bool> {
    Ok(a.schema == b.schema
        && a.user_key_id == b.user_key_id
        && a.limits == b.limits
        && a.rule_hash == b.rule_hash
        && serde_json::to_value(&a.rules)? == serde_json::to_value(&b.rules)?)
}
