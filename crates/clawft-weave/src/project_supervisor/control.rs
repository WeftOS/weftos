//! Watching, restarting and stopping children: the monitor task, restart
//! policy, stop, revoke and rekey.

use super::*;

impl Supervisor {
    pub(super) fn spawn_monitor(self: &Arc<Self>, id: String, slot: Arc<Slot>, generation: u64) {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.monitor(id, slot, generation).await });
    }

    /// Watch one child through its crashes: restart per policy, mark
    /// `failed` when the budget is spent, stop watching on a clean exit or
    /// when a stop or newer start superseded this generation.
    pub(super) async fn monitor(self: Arc<Self>, id: String, slot: Arc<Slot>, mut generation: u64) {
        let mut synthetic: Option<ExitInfo> = None;
        loop {
            let info = match synthetic.take() {
                Some(i) => i,
                None => self.launcher.wait_exit(&id).await,
            };
            let stop_requested = self.launcher.stop_requested(&id);
            let outcome = {
                let mut st = slot.st();
                if st.generation != generation {
                    return;
                }
                st.last_exit = Some(info);
                if stop_requested || info.clean() {
                    None
                } else {
                    let now = Instant::now();
                    match st.tracker.as_mut().map(|t| t.on_crash(now)) {
                        Some(Decision::Restart { after }) => {
                            st.restarts += 1;
                            Some(Ok(after))
                        }
                        Some(Decision::GiveUp { restarts_in_window }) => {
                            let why = format!(
                                "{restarts_in_window} restarts inside the window, last exit code {:?} signal {:?}",
                                info.code, info.signal
                            );
                            st.failed = Some(why.clone());
                            Some(Err(why))
                        }
                        None => Some(Err("no restart policy".to_owned())),
                    }
                }
            };
            match outcome {
                None => {
                    self.chain(
                        "project.kernel.exited",
                        json!({"project_id": id, "code": info.code, "signal": info.signal, "clean": true}),
                    );
                    self.set_state(&id, &slot, ChildState::Stopped);
                    return;
                }
                Some(Err(why)) => {
                    // Chained before the state flips: whoever sees `failed`
                    // can already find the event.
                    self.chain(
                        "project.kernel.failed",
                        json!({"project_id": id, "reason": why}),
                    );
                    self.launcher.revoke_tokens(&id);
                    self.launcher.clean_spawn_file(&id);
                    self.set_state(&id, &slot, ChildState::Failed);
                    return;
                }
                Some(Ok(after)) => {
                    self.set_state(&id, &slot, ChildState::Starting);
                    self.chain(
                        "project.kernel.exited",
                        json!({"project_id": id, "code": info.code, "signal": info.signal,
                               "clean": false, "restart_in_ms": after.as_millis() as u64}),
                    );
                    tokio::time::sleep(after).await;
                    let _g = slot.gate.lock().await;
                    if slot.st().generation != generation {
                        return;
                    }
                    match self.restart_once(&id, &slot).await {
                        Ok(g) => {
                            generation = g;
                            self.chain("project.kernel.restarted", json!({"project_id": id}));
                        }
                        Err(e) => {
                            tracing::warn!(project = %id, error = %e, "project kernel restart failed");
                            synthetic = Some(ExitInfo {
                                code: Some(-1),
                                signal: None,
                                clean_hint: false,
                            });
                        }
                    }
                }
            }
        }
    }

    pub(super) async fn restart_once(self: &Arc<Self>, id: &str, slot: &Arc<Slot>) -> Result<u64, SupError> {
        self.prepare(id)?;
        let handle = slot
            .st()
            .handle
            .clone()
            .ok_or_else(|| SupError::Identity("project kernel was unloaded".into()))?;
        self.driver_host(&handle.runtime)?.start(&handle).await?;
        let g = {
            let mut st = slot.st();
            st.generation += 1;
            if let Some(t) = st.tracker.as_mut() {
                t.on_started(Instant::now());
            }
            st.generation
        };
        // The new child becomes `running` when it answers; the monitor does
        // not wait for that.
        let this = Arc::clone(self);
        let (id2, slot2) = (id.to_owned(), Arc::clone(slot));
        tokio::spawn(async move {
            if this.wait_ready(&id2).await.is_ok() && slot2.st().generation == g {
                this.set_state(&id2, &slot2, ChildState::Running);
                this.note_build(&id2).await;
                this.record_kernel_build(&id2).await;
            }
        });
        Ok(g)
    }

    /// Stop the child gracefully (final anchor, then signals). Stopped
    /// children are not restarted. Returns whether one was running.
    pub async fn stop(self: &Arc<Self>, id: &str) -> Result<bool, SupError> {
        clawft_types::project::validate_id(id).map_err(|e| SupError::InvalidId(e.to_string()))?;
        let slot = self.slot(id);
        let _g = slot.gate.lock().await;
        self.stop_locked(id, &slot, "stop").await
    }

    pub(super) async fn stop_locked(
        self: &Arc<Self>,
        id: &str,
        slot: &Arc<Slot>,
        why: &str,
    ) -> Result<bool, SupError> {
        let running = match self.launcher.probe(id).await {
            ChildProbe::Running { .. } => true,
            ChildProbe::Unverifiable { reason } => {
                self.launcher.revoke_tokens(id);
                return Err(SupError::LiveLeftover(reason));
            }
            _ => false,
        };
        let handle = {
            let mut st = slot.st();
            st.generation += 1; // retire the monitor of the old child
            st.handle.clone()
        };
        // Credentials first: whatever happens to the process, its token is
        // dead before we try to stop it.
        self.launcher.revoke_tokens(id);
        let mut stop_err: Option<String> = None;
        if running {
            if why == "idle" {
                self.set_state(id, slot, ChildState::IdleStopping);
            }
            if let Some(h) = handle
                && let Err(e) = self.driver_host(&h.runtime)?.stop(&h, self.cfg.term_grace).await
            {
                stop_err = Some(e.to_string());
            }
            // The gated stop failed or did not finish: fall through to the
            // launcher's graceful-then-signal path. A project we were asked
            // to stop must not keep running because governance said no.
            match self.launcher.probe(id).await {
                ChildProbe::Running { identity } => {
                    let child = ChildRef {
                        project_id: id.to_owned(),
                        identity,
                    };
                    if let Err(e) = self.launcher.terminate(&child, Duration::ZERO).await {
                        stop_err.get_or_insert(e.to_string());
                    }
                }
                ChildProbe::Unverifiable { reason } => return Err(SupError::LiveLeftover(reason)),
                _ => {}
            }
            let final_probe = self.launcher.probe(id).await;
            if let ChildProbe::Unverifiable { reason } = &final_probe {
                return Err(SupError::LiveLeftover(reason.clone()));
            }
            if !matches!(final_probe, ChildProbe::Running { .. }) {
                self.chain(
                    if why == "idle" {
                        "project.kernel.idle_stop"
                    } else {
                        "project.kernel.stopped"
                    },
                    json!({"project_id": id, "via_fallback": stop_err.is_some()}),
                );
                stop_err = None;
            }
        }
        self.launcher.clean_spawn_file(id);
        // A failed project stays failed until `restart` clears it; a child
        // we could not stop is failed too.
        let cur = {
            let mut st = slot.st();
            if let Some(e) = &stop_err {
                st.failed = Some(format!("could not stop the project kernel: {e}"));
                st.state = ChildState::Failed;
            } else if st.state != ChildState::Failed {
                st.state = ChildState::Stopped;
            }
            st.state
        };
        self.set_state(id, slot, cur);
        match stop_err {
            Some(e) => Err(SupError::Runtime(RuntimeError::Backend(e))),
            None => Ok(running),
        }
    }

    /// `project.restart`: stop, forget the failure and start again.
    pub async fn restart(self: &Arc<Self>, id: &str) -> Result<Running, SupError> {
        clawft_types::project::validate_id(id).map_err(|e| SupError::InvalidId(e.to_string()))?;
        let slot = self.slot(id);
        let _g = slot.gate.lock().await;
        self.stop_locked(id, &slot, "restart").await?;
        {
            let mut st = slot.st();
            st.failed = None;
            st.restarts = 0;
            st.state = ChildState::Stopped;
            if let Some(t) = st.tracker.as_mut() {
                t.reset();
            }
        }
        self.start_locked(id, &slot).await
    }

    /// Stop every running child (user-daemon stop cascade).
    pub async fn stop_all(self: &Arc<Self>) -> Vec<String> {
        // A child that was still booting when the adoption scan ran (its
        // socket not yet bound) is a leftover, not a slot: give it the
        // chance to answer now, so the cascade does not skip it.
        self.reconcile_leftovers().await;
        let ids: Vec<String> = self
            .slots
            .lock()
            .expect("slots lock poisoned")
            .keys()
            .cloned()
            .collect();
        let mut stopped = Vec::new();
        for id in ids {
            if matches!(self.stop(&id).await, Ok(true)) {
                stopped.push(id);
            }
        }
        stopped
    }

    /// A project's key was replaced (`project.rekey`): stop the old child;
    /// the next start runs the rekeyed project normally. No marker.
    pub async fn rekeyed(self: &Arc<Self>, id: &str) {
        if let Err(e) = self.stop(id).await {
            tracing::warn!(project = id, error = %e, "could not stop a rekeyed project's kernel");
        }
    }

    /// `project.revoke` happened (`on_identity_change` wrote the terminal
    /// `<run>/<id>/revoked` marker first): kill the child's credentials first, then
    /// stop it (signals if the gated stop fails) and mark the project failed.
    /// The project is never respawned: `prepare` refuses while the marker
    /// exists.
    pub async fn revoked(self: &Arc<Self>, id: &str, reason: &str) {
        self.launcher.revoke_tokens(id);
        let slot = self.slot(id);
        let _g = slot.gate.lock().await;
        if let Err(e) = self.stop_locked(id, &slot, "revoke").await {
            tracing::warn!(project = id, error = %e, "could not stop a revoked project's kernel");
        }
        {
            let mut st = slot.st();
            st.failed = Some(format!("revoked ({reason})"));
            st.state = ChildState::Failed;
        }
        self.set_state(id, &slot, ChildState::Failed);
    }
}
