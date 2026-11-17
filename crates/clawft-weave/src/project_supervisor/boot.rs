//! Boot-time pieces of the supervisor: adoption of verified leftovers, the
//! idle pass and the `post_boot` wiring for the user daemon.

use super::*;

/// Longest the boot scan waits for children that hold their lock but have
/// not bound their socket yet.
const BOOT_RETRY: Duration = Duration::from_secs(3);
/// Longest `stop_all` waits for them once more.
const CASCADE_RETRY: Duration = Duration::from_secs(2);

impl Supervisor {
    pub(super) async fn prove_child(&self, id: &str, socket: &std::path::Path) -> Result<(), String> {
        use ed25519_dalek::Verifier;
        use rand::RngCore;
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let nonce = hex::encode(raw);
        let signature = self.deps.io.prove(socket, &nonce).await.ok_or("child supplied no project-key proof")?;
        let sig = clawft_types::project::canon::hex_decode::<64>(&signature).ok_or("invalid proof signature")?;
        let view = crate::project_cert_rpc::current_view(&self.deps.cert_env).map_err(|e| e.to_string())?;
        let cert = view.current_cert(id).ok_or("no current project certificate")?;
        let pk = clawft_types::project::canon::hex_decode::<32>(&cert.project_pubkey).ok_or("bad certified project key")?;
        let key = ed25519_dalek::VerifyingKey::from_bytes(&pk).map_err(|e| e.to_string())?;
        key.verify(format!("weftos-project-handshake-v1\n{id}\n{nonce}").as_bytes(),
            &ed25519_dalek::Signature::from_bytes(&sig)).map_err(|e| e.to_string())
    }

    /// Inspect persisted immutable identity and demand a signed answer on
    /// the parent-selected host socket. Uncertainty remains a leftover.
    pub(super) async fn scan_one_container(&self, id: &str) -> Option<Found> {
        let saved = state::read(&self.run_dir(id))?.container?;
        let cid = saved.id.clone();
        let binding = clawft_rpc::mesh_local::ContainerRegistration {
            engine: saved.engine, container_id: cid.clone(),
            host_socket: saved.host_socket.to_string_lossy().into_owned(),
        };
        let host_pid = match self.launcher.verify_container(id, &binding).await {
            Ok(pid) => pid,
            Err(reason) => return Some(Found::UnverifiableContainer { id: id.into(), container_id: cid, reason }),
        };
        let Some(handshake) = self.deps.io.handshake(&saved.host_socket).await else {
            return Some(Found::UnverifiableContainer { id: id.into(), container_id: cid,
                reason: "inspected container did not answer on its host socket".into() });
        };
        if handshake.project_id.as_deref() != Some(id) || handshake.pid == 0 {
            return Some(Found::UnverifiableContainer { id: id.into(), container_id: cid,
                reason: "child handshake named another project or no guest PID".into() });
        }
        if let Err(reason) = self.prove_child(id, &saved.host_socket).await {
            return Some(Found::UnverifiableContainer { id: id.into(), container_id: cid, reason });
        }
        Some(Found::AdoptedContainer { id: id.into(), container_id: cid, host_pid, guest_pid: handshake.pid })
    }

    pub(super) async fn adopt_one_container(self: &Arc<Self>, id: &str, host_pid: u32, guest_pid: u32, slot: &Arc<Slot>) -> Result<(), String> {
        let (w, manifest) = match self.prepare(id) {
            Ok(x) => x,
            Err(SupError::Revoked(_)) => {
                self.launcher.adopt_container(id, host_pid).await?;
                let c = state::read(&self.run_dir(id)).and_then(|s| s.container).ok_or("missing container state")?;
                let stopped = self.launcher.terminate(&ChildRef { project_id: id.into(), identity: ChildIdentity::Container {
                    engine: c.engine, immutable_container_id: c.id, host_pid,
                } }, self.cfg.term_grace).await;
                self.launcher.forget(id);
                return Err(format!("revoked container stop: {stopped:?}"));
            }
            Err(e) => return Err(e.to_string()),
        };
        self.launcher.adopt_container(id, host_pid).await?;
        let handle = self.host.load(&w, &Self::host_cfg(id)).await.map_err(|e| e.to_string())?;
        let generation = {
            let mut st = slot.st();
            st.handle = Some(handle);
            st.tracker = Some(self.tracker_for(&manifest));
            st.generation += 1;
            st.generation
        };
        self.file_adopted_session(id, guest_pid);
        self.set_state(id, slot, ChildState::Running);
        self.spawn_monitor(id.into(), Arc::clone(slot), generation);
        Ok(())
    }
    /// File an expired registry session for an adopted child so it can
    /// re-register without a spawn nonce. Uses the REAL certified project
    /// key: a zero or default key would silently break the child's signed
    /// heartbeats. Without a certificate in force nothing is filed.
    fn file_adopted_session(&self, id: &str, pid: u32) {
        use crate::mesh_local_registry::{NewSession, registry};
        let Ok(view) = crate::project_cert_rpc::current_view(&self.deps.cert_env) else { return };
        let Some(cert) = view.current_cert(id) else { return };
        let Some(project_pubkey) = clawft_types::project::canon::hex_decode::<32>(&cert.project_pubkey) else {
            return;
        };
        registry().adopt_expired(NewSession {
            project_id: id.to_owned(),
            socket: self.socket(id),
            pid,
            container: state::read(&self.run_dir(id)).and_then(|s| s.container).map(|c| clawft_rpc::mesh_local::ContainerRegistration {
                engine: c.engine, container_id: c.id, host_socket: c.host_socket.to_string_lossy().into_owned(),
            }),
            addresses: vec![id.to_owned()],
            topic_prefixes: vec![format!("chain/{id}/")],
            version: env!("CARGO_PKG_VERSION").to_owned(),
            project_key_id: cert.project_key_id.clone(),
            project_pubkey,
        });
    }

    /// Adopt the verified child `pid` of `id` into supervision. The project
    /// gate must be held. A verified child the supervisor will not manage is
    /// never left silently running: a revoked project's child is stopped
    /// (identity re-checked before any signal), anything else is reported
    /// with the reason.
    pub(super) async fn adopt_one(
        self: &Arc<Self>,
        id: &str,
        pid: u32,
        slot: &Arc<Slot>,
    ) -> Result<(), adopt::Skip> {
        let (w, manifest) = match self.prepare(id) {
            Ok(x) => x,
            Err(e) => return Err(self.refuse_adopted(id, pid, e).await),
        };
        self.launcher.adopt(id, pid);
        let handle = match self
            .driver_host(self.selected_adapter(id).map_err(|e| adopt::Skip::Refused(e.to_string()))?)
            .map_err(|e| adopt::Skip::Refused(e.to_string()))?
            .load(&w, &Self::host_cfg(id))
            .await
        {
            Ok(h) => h,
            Err(e) => {
                self.launcher.forget(id);
                return Err(adopt::Skip::Refused(format!(
                    "verified kernel pid {pid} not managed (load refused: {e}); it was left running"
                )));
            }
        };
        let g = {
            let mut st = slot.st();
            st.handle = Some(handle);
            st.tracker = Some(self.tracker_for(&manifest));
            st.generation += 1;
            st.generation
        };
        self.file_adopted_session(id, pid);
        self.set_state(id, slot, ChildState::Running);
        self.note_build(id).await;
        self.chain("project.kernel.adopted", json!({"project_id": id, "pid": pid}));
        self.spawn_monitor(id.to_owned(), Arc::clone(slot), g);
        Ok(())
    }

    async fn refuse_adopted(&self, id: &str, pid: u32, e: SupError) -> adopt::Skip {
        if !matches!(e, SupError::Revoked(_)) {
            tracing::warn!(project = %id, error = %e, "verified project kernel not managed");
            return adopt::Skip::Refused(format!("verified kernel pid {pid} not managed: {e}; it was left running"));
        }
        // Revoked: it must not keep running. Terminate re-verifies pid, exe
        // and lock before any signal, so a recycled pid is never hit.
        self.launcher.adopt(id, pid);
        let stopped = self
            .launcher
            .terminate(&ChildRef { project_id: id.to_owned(), identity: ChildIdentity::Native { host_pid: pid } }, self.cfg.term_grace)
            .await;
        let alive = self.launcher.pid_of(id).is_some();
        self.launcher.forget(id);
        self.launcher.revoke_tokens(id);
        self.chain("project.kernel.stopped", json!({"project_id": id, "pid": pid, "reason": "revoked at adoption"}));
        adopt::Skip::Refused(if alive || stopped.is_err() {
            format!("project is revoked and kernel pid {pid} could NOT be stopped; stop it by hand")
        } else {
            format!("project is revoked; kernel pid {pid} was stopped")
        })
    }

    /// Verify and adopt children left by an earlier daemon (see [`adopt`]).
    pub async fn adopt_on_boot(self: &Arc<Self>) -> Vec<Found> {
        let mut found = Vec::new();
        for id in self.run_ids() {
            if let Some(one) = self.scan_project(&id).await {
                found.push(one);
            }
        }
        if let Ok(dirs) = std::fs::read_dir(&self.cfg.run_root) {
            for dir in dirs.flatten() {
                let id = dir.file_name().to_string_lossy().into_owned();
                if clawft_types::project::validate_id(&id).is_ok()
                    && let Some(one) = self.scan_one_container(&id).await {
                    found.push(one);
                }
            }
        }
        for f in &mut found {
            match f {
                Found::Adopted { id, pid } => {
                    let (id2, pid2) = (id.clone(), *pid);
                    let slot = self.slot(&id2);
                    let _g = slot.gate.lock().await;
                    if let Err(reason) = self.adopt_one(&id2, pid2, &slot).await {
                        *f = Found::Unverifiable { id: id2, pid: Some(pid2), reason };
                    }
                }
                Found::Unverifiable { id, pid, reason } if *reason != adopt::Skip::Dead => {
                    tracing::warn!(project = %id, pid = ?pid, %reason, "leftover project kernel not adopted and not signalled");
                }
                Found::Unverifiable { .. } => {}
                Found::AdoptedContainer { id, container_id, host_pid, guest_pid } => {
                    let (id2, cid, host, guest) = (id.clone(), container_id.clone(), *host_pid, *guest_pid);
                    let slot = self.slot(&id2);
                    let _g = slot.gate.lock().await;
                    if let Err(reason) = self.adopt_one_container(&id2, host, guest, &slot).await {
                        *f = Found::UnverifiableContainer { id: id2, container_id: cid, reason };
                    }
                }
                Found::UnverifiableContainer { id, container_id, reason } => {
                    tracing::warn!(project = %id, container = %container_id, %reason, "container left unverified; duplicate launch blocked");
                }
            }
        }
        self.retry_handshake_leftovers(&mut found, self.cfg.ready_timeout.min(BOOT_RETRY)).await;
        *self.leftovers.lock().expect("leftovers lock poisoned") = found.clone();
        found
    }

    /// Leftovers whose pid, executable and lock verified but whose socket did
    /// not answer yet (a child still booting when its daemon restarted):
    /// look again every `ready_poll` for up to `budget` (short: a wedged one
    /// must not hold up boot or a stop cascade), adopting the
    /// ones that now answer. Anything else stays as filed.
    async fn retry_handshake_leftovers(self: &Arc<Self>, found: &mut [Found], budget: Duration) {
        let waiting = |f: &Found| {
            matches!(f, Found::Unverifiable { pid: Some(_), reason: adopt::Skip::HandshakeFailed(_), .. })
        };
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let mut pending = false;
            for f in found.iter_mut().filter(|f| waiting(f)) {
                let Found::Unverifiable { id, .. } = f else { continue };
                let id = id.clone();
                let rescan = self.scan_project(&id).await;
                match rescan {
                    Some(Found::Adopted { id, pid }) => {
                        let slot = self.slot(&id);
                        let _g = slot.gate.lock().await;
                        *f = match self.adopt_one(&id, pid, &slot).await {
                            Ok(()) => Found::Adopted { id, pid },
                            Err(reason) => Found::Unverifiable { id, pid: Some(pid), reason },
                        };
                    }
                    Some(other) => {
                        pending |= waiting(&other);
                        *f = other;
                    }
                    None => {}
                }
            }
            if !pending || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(self.cfg.ready_poll).await;
        }
    }

    /// [`retry_handshake_leftovers`](Self::retry_handshake_leftovers) over
    /// the leftovers the last scan kept.
    pub(super) async fn reconcile_leftovers(self: &Arc<Self>) {
        let mut left = self.leftovers.lock().expect("leftovers lock poisoned").clone();
        if left.iter().any(|f| {
            matches!(f, Found::Unverifiable { pid: Some(_), reason: adopt::Skip::HandshakeFailed(_), .. })
        }) {
            self.retry_handshake_leftovers(&mut left, self.cfg.ready_timeout.min(CASCADE_RETRY)).await;
            *self.leftovers.lock().expect("leftovers lock poisoned") = left;
        }
    }

    /// One liveness pass at `now`: a `running` child whose registry session
    /// has stayed expired (three missed heartbeats, no new registration) for
    /// `lost_heartbeat_grace` is wedged or cut off. It is treated as a crash:
    /// stopped (pid re-verified before any signal) and restarted inside its
    /// restart budget, or marked `failed` when the budget is spent. Returns
    /// the ids acted on. A child whose last beat said it was busy gets
    /// `lost_heartbeat_busy_ceiling` instead of the grace, so a child wedged
    /// while busy is still restarted. An adopted child that has not
    /// registered is only reported (`Status::unregistered_secs`).
    pub async fn liveness_pass(self: &Arc<Self>, now: Instant) -> Vec<String> {
        let running: Vec<String> = self
            .status_all()
            .await
            .into_iter()
            .filter(|s| s.state == ChildState::Running)
            .map(|s| s.project_id)
            .collect();
        let mut acted = Vec::new();
        for id in running {
            let slot = self.slot(&id);
            let due = {
                let lost = self.deps.activity.lost_heartbeat(&id);
                let lost_busy = !lost && self.deps.activity.lost_heartbeat_busy(&id);
                let unregistered = self.deps.activity.unregistered_adopted(&id);
                let mut st = slot.st();
                // An adopted child that never re-registers is never restarted
                // (its tombstone is not a lost heartbeat): surface it instead.
                if unregistered {
                    st.unregistered_since.get_or_insert(now);
                } else {
                    st.unregistered_since = None;
                }
                if !lost && !lost_busy {
                    st.expired_since = None;
                    false
                } else {
                    // A busy last beat earns the long ceiling, not immunity.
                    let need = if lost { self.cfg.lost_heartbeat_grace } else { self.busy_ceiling(&id) };
                    let since = *st.expired_since.get_or_insert(now);
                    now.saturating_duration_since(since) >= need
                }
            };
            // One restart per pass: if the registry itself stalled, every
            // child looks lost at once and must not be restarted together.
            if due && self.restart_lost(&id, &slot).await {
                acted.push(id);
                break;
            }
        }
        acted
    }

    /// The busy-skip ceiling for `id`: its manifest's
    /// `[serve] lost_heartbeat_busy_ceiling_secs`, else the daemon default;
    /// never below the plain grace.
    fn busy_ceiling(&self, id: &str) -> std::time::Duration {
        let own = clawft_types::project::find_by_id(&self.cfg.manifests_dir, id)
            .ok()
            .flatten()
            .and_then(|m| m.serve)
            .and_then(|s| s.lost_heartbeat_busy_ceiling_secs)
            .map(std::time::Duration::from_secs);
        own.unwrap_or(self.cfg.lost_heartbeat_busy_ceiling).max(self.cfg.lost_heartbeat_grace)
    }

    async fn restart_lost(self: &Arc<Self>, id: &str, slot: &Arc<Slot>) -> bool {
        let _g = slot.gate.lock().await;
        // Re-check under the gate: a stop, restart or re-registration may
        // have happened while this pass waited for it.
        if slot.st().state != ChildState::Running
            || !(self.deps.activity.lost_heartbeat(id) || self.deps.activity.lost_heartbeat_busy(id))
        {
            return false;
        }
        let (decision, old_pid) = {
            let decision = {
                let mut st = slot.st();
                st.expired_since = None;
                st.tracker.as_mut().map(|t| t.on_crash(Instant::now()))
            };
            (decision, self.probe_running(id).await)
        };
        if let Err(e) = self.stop_locked(id, slot, "heartbeat").await {
            tracing::warn!(project = %id, error = %e, "could not stop a child that lost its heartbeat");
            return false;
        }
        self.chain(
            "project.kernel.exited",
            json!({"project_id": id, "pid": old_pid, "clean": false, "reason": "heartbeat lost"}),
        );
        match decision {
            Some(Decision::Restart { .. }) => {
                slot.st().restarts += 1;
                match self.start_locked(id, slot).await {
                    Ok(_) => {
                        self.chain("project.kernel.restarted", json!({"project_id": id, "reason": "heartbeat lost"}));
                        true
                    }
                    Err(e) => {
                        tracing::warn!(project = %id, error = %e, "restart after a lost heartbeat failed");
                        false
                    }
                }
            }
            other => {
                let why = match other {
                    Some(Decision::GiveUp { restarts_in_window }) => {
                        format!("{restarts_in_window} restarts inside the window, last stop: heartbeat lost")
                    }
                    _ => "no restart policy".to_owned(),
                };
                slot.st().failed = Some(why.clone());
                self.chain("project.kernel.failed", json!({"project_id": id, "reason": why}));
                self.launcher.revoke_tokens(id);
                self.launcher.clean_spawn_file(id);
                slot.st().state = ChildState::Failed;
                self.set_state(id, slot, ChildState::Failed);
                true
            }
        }
    }

    /// One idle pass at `now_unix`: stop every running project that has been
    /// quiet for its `idle_stop_secs`. Returns the ids stopped.
    pub async fn idle_pass(self: &Arc<Self>, now_unix: u64) -> Vec<String> {
        let running: Vec<String> = self
            .status_all()
            .await
            .into_iter()
            .filter(|s| s.state == ChildState::Running)
            .map(|s| s.project_id)
            .collect();
        let mut stopped = Vec::new();
        for id in running {
            let Ok(Some(m)) = clawft_types::project::find_by_id(&self.cfg.manifests_dir, &id) else {
                continue;
            };
            let secs = m.serve.as_ref().map_or(0, |s| s.idle_stop_secs());
            let activity = self.deps.activity.activity(&id);
            if idle::should_stop(now_unix, secs, activity.as_ref()) {
                let slot = self.slot(&id);
                let _g = slot.gate.lock().await;
                if slot.st().state == ChildState::Running
                    && matches!(self.stop_locked(&id, &slot, "idle").await, Ok(true))
                {
                    stopped.push(id);
                }
            }
        }
        stopped
    }

    /// Run [`idle_pass`](Self::idle_pass) every `idle_poll` until the
    /// supervisor is dropped.
    pub fn spawn_idle_loop(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let every = self.cfg.idle_poll;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                let Some(this) = weak.upgrade() else { return };
                this.idle_pass(state::now_unix()).await;
                this.liveness_pass(Instant::now()).await;
            }
        });
    }
}

/// `post_boot` body for the user daemon: build the supervisor from the
/// booted kernel, install it, adopt leftovers and start the idle loop. A
/// no-op for any other profile.
pub fn post_boot(kernel: &clawft_kernel::Kernel<clawft_platform::NativePlatform>) {
    if !crate::user_daemon::is_active() {
        return;
    }
    let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
    let Some(chain) = kernel.chain_manager().cloned() else { return };
    let Some(user_key) = chain.signing_key_clone() else { return };
    let (Some(home), Some(manifests_dir)) = (
        clawft_types::runtime_paths::home_dir(),
        crate::project_rpc::configured_dir(),
    ) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else { return };
    // The run root the revoked-marker writer uses too (one derivation).
    let Some(run_root) = crate::project_cert_rpc::user_run_root() else { return };
    let mut cfg = SupervisorConfig::new(&home, exe);
    cfg.parent_socket = crate::user_daemon::child_socket_path(&run_root);
    cfg.run_root = run_root;
    cfg.manifests_dir = manifests_dir.clone();
    let gate = kernel.governance_gate().cloned();
    // The parent's real caps, so a child's merged limits start from them
    // (the overlay can only tighten). Boot-time values, like the caps.
    let kc = kernel.kernel_config();
    let parent_limits = clawft_kernel::gate::parent_limits_of(kc);
    let deps = Deps {
        cert_env: CertEnv { chain, user_key: user_key.clone(), manifests_dir: manifests_dir.clone() },
        snapshot: Arc::new(move || {
            gate.as_ref().and_then(|g| g.governance_snapshot()).map(|mut s| {
                s.limits = parent_limits;
                s
            })
        }),
        tokens: crate::token_rpc::authority_for_kernel(kernel),
        activity: Arc::new(idle::RegistryActivity),
        io: Arc::new(io::RpcChildIo::new(user_key, manifests_dir)),
        gate: None,
    };
    let sup = Supervisor::new(cfg, deps);
    if !install_global(Arc::clone(&sup)) {
        return;
    }
    rt.spawn(async move {
        let found = sup.adopt_on_boot().await;
        tracing::info!(children = found.len(), "project supervisor adoption scan done");
        sup.spawn_idle_loop();
    });
}
