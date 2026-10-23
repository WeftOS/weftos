//! Start, stop, unload, status, restart and reconcile for [`InferRuntime`].
//!
//! The instances lock is never held across a network call or a process
//! termination: state is read or taken under the lock, the slow work runs
//! with the lock dropped, and results are written back under a fresh lock.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::exposure::{port_in_use, reachable_beyond_loopback};
use super::ollama::{self, LoadState};
use super::probe::{Health, ServerClient, ServerReport};
use super::runtime::{InferRuntime, Instance, Managed, ManagedPlan};
use super::spec::InferFlavor;
use crate::workload_runtime::evidence::RunEvidence;
use crate::workload_runtime::supervise::{LaunchSpec, ProcLimits, Supervised};
use crate::workload_runtime::types::{
    InstanceHandle, InstanceState, InstanceStatus, RuntimeError, WorkloadRuntime,
};

/// Output kept per stream from a model server (bytes).
const OUTPUT_LIMIT: usize = 1024 * 1024;
/// Grace given to a server when its instance is unloaded.
const UNLOAD_GRACE: Duration = Duration::from_secs(5);
/// Timeout of an Ollama load request (a large model can take minutes).
const OLLAMA_LOAD_TIMEOUT: Duration = Duration::from_secs(900);

/// What [`InferRuntime::reconcile`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconcile {
    /// Running and answering, or not wanted running.
    Healthy,
    /// Not a supervised process (adopted, or Ollama).
    NotManaged,
    /// It died; the restart waits out its backoff.
    Backoff {
        /// Time left before the next restart is allowed.
        retry_in: Duration,
    },
    /// It was restarted.
    Restarted {
        /// Restarts so far, this one included.
        attempt: u32,
    },
    /// The restart budget is spent; the instance stays `Exited`.
    GaveUp {
        /// Restarts attempted.
        attempts: u32,
    },
    /// The server was listening beyond loopback, so it was stopped. The
    /// caller chains this; the instance stays down until reloaded.
    StoppedExposed {
        /// Local addresses the port answered on.
        reachable_on: Vec<IpAddr>,
    },
}

fn unknown(h: &InstanceHandle) -> RuntimeError {
    RuntimeError::UnknownInstance(h.instance_id.clone())
}

fn unlimited() -> ProcLimits {
    // Model servers map tens of GiB of weights and many files; the cog
    // limits do not apply. Large finite values: an explicit RLIM_INFINITY
    // is refused by some kernels when set as the hard limit.
    ProcLimits {
        mem_bytes: 1 << 46,
        cpu_secs: None,
        nofile: 4096,
        fsize_bytes: 1 << 44,
    }
}

fn spawn(rt: &InferRuntime, inst: &mut Instance) -> Result<(), RuntimeError> {
    let run_as = rt.managed_cfg().and_then(|m| m.run_as);
    let Some(Managed {
        plan: ManagedPlan::Process(l),
        proc,
        wanted,
        exited_at,
        started_at,
        exposed,
        ..
    }) = inst.managed.as_mut()
    else {
        return Err(RuntimeError::Unsupported("not a supervised process".into()));
    };
    if let Some(a) = exposed {
        return Err(RuntimeError::InvalidState(format!(
            "the server was stopped for listening beyond loopback ({a:?}); reload the instance"
        )));
    }
    *proc = Some(Supervised::spawn(LaunchSpec {
        program: l.program.clone(),
        args: l.args.clone(),
        env: l.env.clone(),
        cwd: l.dir.clone(),
        limits: unlimited(),
        output_limit: OUTPUT_LIMIT,
        run_as,
    })?);
    *wanted = true;
    *exited_at = None;
    *started_at = Some(Instant::now());
    Ok(())
}

/// Under the lock: if the process exited on its own, take it (and its exit
/// code) so it can be collected after the lock is dropped.
fn take_exited(mg: &mut Managed) -> Option<(Supervised, Option<i32>)> {
    let code = mg.proc.as_mut()?.try_exit()?.code();
    mg.exited_at.get_or_insert_with(Instant::now);
    Some((mg.proc.take()?, code))
}

fn map_report(
    flavor: InferFlavor,
    model: Option<&str>,
    r: &ServerReport,
    alive: bool,
    adopted: bool,
    load: Option<&LoadState>,
) -> InstanceStatus {
    let degraded = |d: String| InstanceStatus {
        state: InstanceState::Degraded,
        exit_code: None,
        detail: Some(d),
    };
    let running = |d: &str| InstanceStatus {
        detail: Some(d.into()),
        ..InstanceStatus::of(InstanceState::Running)
    };
    match &r.health {
        Health::Loading => degraded("loading model".into()),
        Health::Unhealthy(w) => degraded(w.clone()),
        Health::Unreachable(w) if alive => degraded(format!("starting: not listening yet ({w})")),
        Health::Unreachable(w) => InstanceStatus {
            state: InstanceState::Exited,
            exit_code: None,
            detail: Some(if adopted {
                format!("unreachable ({w}); adopted servers are not restarted")
            } else {
                format!("unreachable ({w})")
            }),
        },
        Health::Up => match (flavor, model) {
            (InferFlavor::Ollama, Some(m)) => {
                if !r.lists(m) {
                    degraded(format!("model {m} is not pulled"))
                } else if r.resident(m) {
                    running("model resident")
                } else if let Some(LoadState::Failed(e)) = load {
                    degraded(format!("load failed: {e}"))
                } else if matches!(load, Some(LoadState::Loading)) {
                    degraded("loading model".into())
                } else if adopted {
                    running("server up; model not resident")
                } else {
                    InstanceStatus::of(InstanceState::Loaded)
                }
            }
            (_, Some(m)) if !r.models.is_empty() && !r.lists(m) => {
                degraded(format!("model {m} is not listed by the server"))
            }
            _ => InstanceStatus::of(InstanceState::Running),
        },
    }
}

impl InferRuntime {
    fn rt_id(&self) -> String {
        WorkloadRuntime::id(self).to_string()
    }

    /// Write back the evidence of a process collected outside the lock.
    async fn store_last(&self, h: &InstanceHandle, mut ev: RunEvidence) {
        ev.runtime = self.rt_id();
        ev.instance_id = h.instance_id.clone();
        if let Some(mg) = self
            .instances
            .lock()
            .await
            .get_mut(&h.instance_id)
            .and_then(|i| i.managed.as_mut())
        {
            mg.last = Some(ev);
        }
    }

    pub(super) async fn start_instance(&self, h: &InstanceHandle) -> Result<(), RuntimeError> {
        // Phase 1: what to do, under the lock.
        let (client, spec, supervised) = {
            let g = self.instances.lock().await;
            let inst = g.get(&h.instance_id).ok_or_else(|| unknown(h))?;
            if let Some(a) = inst.managed.as_ref().and_then(|m| m.exposed.as_ref()) {
                return Err(RuntimeError::InvalidState(format!(
                    "the server was stopped for listening beyond loopback ({a:?}); reload the instance"
                )));
            }
            let supervised = inst
                .managed
                .as_ref()
                .map(|m| matches!(m.plan, ManagedPlan::Process(_)));
            (inst.client.clone(), inst.spec.clone(), supervised)
        };
        match supervised {
            None => {
                // Adopted: nothing to start; confirm the server answers.
                return match client.probe(spec.runtime).await.health {
                    Health::Unreachable(w) => Err(RuntimeError::Backend(format!(
                        "adopted server is not answering ({w}); WeftOS does not start adopted servers"
                    ))),
                    _ => Ok(()),
                };
            }
            Some(true) => {
                // Phase 2: never start over something already listening.
                let (ip, port) = (spec.bind_ip()?, spec.port()?);
                if port_in_use(ip, port).await {
                    return Err(RuntimeError::Backend(format!(
                        "{ip}:{port} is already taken by something else; not starting over it"
                    )));
                }
            }
            Some(false) => {}
        }
        // Unified-memory budget and co-residency (managed only): refuse
        // before anything is spawned.
        let mut reserved_new = false;
        if supervised.is_some()
            && let Some(ledger) = self.managed_cfg().and_then(|m| m.ledger.clone())
        {
            match ledger.reserve(&h.instance_id, &spec) {
                Ok(newly) => reserved_new = newly,
                Err(e) => return Err(RuntimeError::AdmissionRefused(e.to_string())),
            }
        }
        let result: Result<(), RuntimeError> = async {
            // Phase 3: act, under the lock (no awaits).
            let mut g = self.instances.lock().await;
            let inst = g.get_mut(&h.instance_id).ok_or_else(|| unknown(h))?;
            let Some(mg) = inst.managed.as_mut() else {
                return Err(unknown(h));
            };
            match &mg.plan {
                ManagedPlan::Process(_) => {
                    if mg.proc.as_mut().is_some_and(|p| p.try_exit().is_none()) {
                        return Err(RuntimeError::InvalidState("already running".into()));
                    }
                    mg.proc = None;
                    spawn(self, inst)
                }
                ManagedPlan::Ollama { tag } => {
                    if matches!(&mg.load, Some((s, _))
                        if matches!(*s.lock().unwrap_or_else(|e| e.into_inner()), LoadState::Loading))
                    {
                        return Err(RuntimeError::InvalidState("already loading".into()));
                    }
                    let keep = self
                        .managed_cfg()
                        .map_or("5m".to_string(), |m| m.keep_alive.clone());
                    let client =
                        ServerClient::new(inst.client.base().to_string(), OLLAMA_LOAD_TIMEOUT)?;
                    let state = std::sync::Arc::new(std::sync::Mutex::new(LoadState::Loading));
                    let (s2, tag) = (state.clone(), tag.clone());
                    let we_loaded = mg.we_loaded.clone();
                    let ledger = self.managed_cfg().and_then(|m| m.ledger.clone());
                    let instance = h.instance_id.clone();
                    let task = tokio::spawn(async move {
                        // A model somebody else already holds in memory is not
                        // ours to load or to unload later.
                        let r = if ollama::resident(&client, &tag).await {
                            we_loaded.store(false, std::sync::atomic::Ordering::SeqCst);
                            Ok(())
                        } else {
                            // Ours from the moment we ask, so a load that is
                            // aborted half way is still unloaded at stop.
                            we_loaded.store(true, std::sync::atomic::Ordering::SeqCst);
                            let r = ollama::load(&client, &tag, &keep).await;
                            if r.is_err() {
                                we_loaded.store(false, std::sync::atomic::Ordering::SeqCst);
                            }
                            r
                        };
                        if r.is_err()
                            && let Some(l) = ledger
                        {
                            // A failed load holds no memory.
                            l.release(&instance);
                        }
                        *s2.lock().unwrap_or_else(|e| e.into_inner()) =
                            r.map_or_else(LoadState::Failed, |()| LoadState::Done);
                    });
                    mg.load = Some((state, task));
                    mg.wanted = true;
                    Ok(())
                }
            }
        }
        .await;
        if result.is_err()
            && reserved_new
            && let Some(ledger) = self.managed_cfg().and_then(|m| m.ledger.clone())
        {
            ledger.release(&h.instance_id);
        }
        result
    }

    /// Give a managed instance a fresh restart budget. Only an explicit
    /// operator start calls this: an automatic restart or drive never does,
    /// so a server that keeps dying stays given up.
    pub async fn reset_restarts(&self, h: &InstanceHandle) {
        if let Some(mg) = self
            .instances
            .lock()
            .await
            .get_mut(&h.instance_id)
            .and_then(|i| i.managed.as_mut())
        {
            mg.restarts = 0;
        }
    }

    /// Give back the instance's budget (no-op without a ledger).
    fn release_residency(&self, h: &InstanceHandle) {
        if let Some(l) = self.managed_cfg().and_then(|m| m.ledger.as_ref()) {
            l.release(&h.instance_id);
        }
    }

    /// Stop the instance. For Ollama this unloads the model from Ollama's
    /// memory when this adapter loaded it (a model that was already
    /// resident belongs to whoever loaded it): Ollama
    /// has no per-client ownership of a loaded model.
    pub(super) async fn stop_instance(
        &self,
        h: &InstanceHandle,
        grace: Duration,
    ) -> Result<RunEvidence, RuntimeError> {
        // Short-lived, built and matched once; boxing would only add an allocation.
        #[allow(clippy::large_enum_variant)]
        enum Todo {
            Proc(Supervised),
            Ollama(ServerClient, String, bool),
        }
        let todo = {
            let mut g = self.instances.lock().await;
            let inst = g.get_mut(&h.instance_id).ok_or_else(|| unknown(h))?;
            let client = inst.client.clone();
            let Some(mg) = inst.managed.as_mut() else {
                return Err(RuntimeError::Unsupported(format!(
                    "{} is adopted: WeftOS observes it and never stops it; unload forgets the registration",
                    h.instance_id
                )));
            };
            mg.wanted = false;
            match &mg.plan {
                ManagedPlan::Process(_) => Todo::Proc(
                    mg.proc
                        .take()
                        .ok_or_else(|| RuntimeError::InvalidState("not running".into()))?,
                ),
                ManagedPlan::Ollama { tag } => {
                    if let Some((_, t)) = mg.load.take() {
                        t.abort();
                    }
                    Todo::Ollama(
                        client,
                        tag.clone(),
                        mg.we_loaded.swap(false, std::sync::atomic::Ordering::SeqCst),
                    )
                }
            }
        };
        // The memory is given back only once the server is really gone.
        match todo {
            Todo::Proc(p) => {
                let ev = p.terminate(grace).await;
                self.release_residency(h);
                if let Some(mg) = self
                    .instances
                    .lock()
                    .await
                    .get_mut(&h.instance_id)
                    .and_then(|i| i.managed.as_mut())
                {
                    mg.exited_at = Some(Instant::now());
                }
                self.store_last(h, ev.clone()).await;
                let mut ev = ev;
                ev.runtime = self.rt_id();
                ev.instance_id = h.instance_id.clone();
                Ok(ev)
            }
            Todo::Ollama(client, tag, ours) => {
                // Only a model this adapter loaded is taken out of memory.
                if ours {
                    ollama::unload(&client, &tag)
                        .await
                        .map_err(RuntimeError::Backend)?;
                }
                self.release_residency(h);
                Ok(RunEvidence {
                    runtime: self.rt_id(),
                    instance_id: h.instance_id.clone(),
                    ..RunEvidence::default()
                })
            }
        }
    }

    /// Forget the instance. A managed process it spawned is stopped; for
    /// Ollama the model is unloaded (even if another client loaded it).
    pub(super) async fn unload_instance(&self, h: &InstanceHandle) -> Result<(), RuntimeError> {
        let mut inst = self
            .instances
            .lock()
            .await
            .remove(&h.instance_id)
            .ok_or_else(|| unknown(h))?;
        if let Some(mg) = inst.managed.as_mut() {
            // Only ever a process this adapter spawned itself.
            if let Some(p) = mg.proc.take() {
                p.terminate(UNLOAD_GRACE).await;
            }
            if let Some((_, t)) = mg.load.take() {
                t.abort();
            }
            match &mg.plan {
                ManagedPlan::Process(l) => {
                    let _ = std::fs::remove_dir_all(&l.dir);
                }
                ManagedPlan::Ollama { tag } => {
                    // Best effort, and only a model this adapter loaded.
                    if mg.we_loaded.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        let _ = ollama::unload(&inst.client, tag).await;
                    }
                }
            }
        }
        // The instance is forgotten here, so its memory is released even if
        // the best-effort unload above failed: there is no handle left to
        // release it later.
        self.release_residency(h);
        Ok(())
    }

    /// If the instance's server answers on a non-loopback address of this
    /// machine, stop it (it is our process) and remember why.
    async fn enforce_loopback(&self, h: &InstanceHandle) -> Option<Vec<IpAddr>> {
        let port = {
            let g = self.instances.lock().await;
            let inst = g.get(&h.instance_id)?;
            let mg = inst.managed.as_ref()?;
            (matches!(mg.plan, ManagedPlan::Process(_)) && mg.proc.is_some())
                .then(|| inst.spec.port().ok())
                .flatten()?
        };
        let beyond = reachable_beyond_loopback(port).await;
        if beyond.is_empty() {
            return None;
        }
        let p = {
            let mut g = self.instances.lock().await;
            let mg = g.get_mut(&h.instance_id)?.managed.as_mut()?;
            mg.wanted = false;
            mg.exposed = Some(beyond.clone());
            mg.exited_at = Some(Instant::now());
            mg.proc.take()
        };
        if let Some(p) = p {
            let ev = p.terminate(Duration::from_secs(3)).await;
            self.store_last(h, ev).await;
        }
        // Stopped for good (until reloaded): it holds no memory any more.
        self.release_residency(h);
        tracing::warn!(instance = %h.instance_id, ?beyond, "model server listened beyond loopback; stopped");
        Some(beyond)
    }

    pub(super) async fn status_of(&self, h: &InstanceHandle) -> InstanceStatus {
        let (client, spec, model, alive, load, adopted) = {
            let mut g = self.instances.lock().await;
            let Some(inst) = g.get_mut(&h.instance_id) else {
                return InstanceStatus::of(InstanceState::Unknown);
            };
            let (client, spec) = (inst.client.clone(), inst.spec.clone());
            match inst.managed.as_mut() {
                None => {
                    let m = spec.model.clone();
                    (client, spec, m, false, None, true)
                }
                Some(mg) => {
                    if let Some(a) = &mg.exposed {
                        return InstanceStatus {
                            state: InstanceState::Exited,
                            exit_code: None,
                            detail: Some(format!(
                                "stopped: the server listened beyond loopback (reachable on {a:?})"
                            )),
                        };
                    }
                    if let Some((p, code)) = take_exited(mg) {
                        drop(g);
                        let ev = p.terminate(Duration::ZERO).await;
                        self.store_last(h, ev).await;
                        return InstanceStatus {
                            state: InstanceState::Exited,
                            exit_code: code,
                            detail: Some("server process exited".into()),
                        };
                    }
                    let alive = mg.proc.is_some();
                    if matches!(mg.plan, ManagedPlan::Process(_)) && !alive {
                        let state = if mg.last.is_some() {
                            InstanceState::Exited
                        } else {
                            InstanceState::Loaded
                        };
                        return InstanceStatus {
                            state,
                            exit_code: mg.last.as_ref().and_then(|e| e.exit_code),
                            detail: None,
                        };
                    }
                    let load = mg
                        .load
                        .as_ref()
                        .map(|(s, _)| s.lock().unwrap_or_else(|e| e.into_inner()).clone());
                    let model = match &mg.plan {
                        ManagedPlan::Ollama { tag } => tag.clone(),
                        ManagedPlan::Process(_) => mg.model_name.clone(),
                    };
                    (client, spec, Some(model), alive, load, false)
                }
            }
        };
        // The binding is checked before the (slow) health probe: a server
        // we started must not answer beyond loopback, whatever its health.
        if alive && let Some(a) = self.enforce_loopback(h).await {
            return InstanceStatus {
                state: InstanceState::Exited,
                exit_code: None,
                detail: Some(format!(
                    "stopped: the server listened beyond loopback (reachable on {a:?})"
                )),
            };
        }
        let r = client.probe(spec.runtime).await;
        map_report(
            spec.runtime,
            model.as_deref(),
            &r,
            alive,
            adopted,
            load.as_ref(),
        )
    }

    /// Stop (when running) then start again.
    pub async fn restart(&self, h: &InstanceHandle, grace: Duration) -> Result<(), RuntimeError> {
        match self.stop_instance(h, grace).await {
            Ok(_) | Err(RuntimeError::InvalidState(_)) => {}
            Err(e) => return Err(e),
        }
        self.start_instance(h).await
    }

    /// Restart a managed server that died while it should be running,
    /// under the configured backoff and budget, and stop one found
    /// listening beyond loopback. Meant to be called on a timer; each call
    /// does at most one restart.
    pub async fn reconcile(&self, h: &InstanceHandle) -> Result<Reconcile, RuntimeError> {
        let Some(policy) = self.managed_cfg().map(|m| m.restart) else {
            return Ok(Reconcile::NotManaged);
        };
        enum Next {
            Done(Reconcile),
            Restart {
                ip: IpAddr,
                port: u16,
            },
            Check {
                client: ServerClient,
                flavor: InferFlavor,
            },
        }
        let reaped;
        let next = {
            let mut g = self.instances.lock().await;
            let inst = g.get_mut(&h.instance_id).ok_or_else(|| unknown(h))?;
            let (client, flavor) = (inst.client.clone(), inst.spec.runtime);
            let (ip, port) = (inst.spec.bind_ip()?, inst.spec.port()?);
            let Some(mg) = inst.managed.as_mut() else {
                return Ok(Reconcile::NotManaged);
            };
            if !matches!(mg.plan, ManagedPlan::Process(_)) {
                return Ok(Reconcile::NotManaged);
            }
            if let Some(a) = &mg.exposed {
                let reachable_on = a.clone();
                drop(g);
                self.release_residency(h);
                return Ok(Reconcile::StoppedExposed { reachable_on });
            }
            if !mg.wanted {
                return Ok(Reconcile::Healthy);
            }
            reaped = take_exited(mg).map(|(p, _)| p);
            if mg.proc.is_some() {
                Next::Check { client, flavor }
            } else if mg.restarts >= policy.max_restarts {
                Next::Done(Reconcile::GaveUp {
                    attempts: mg.restarts,
                })
            } else {
                let wait = policy.delay(mg.restarts);
                let since = mg.exited_at.map_or(wait, |t| t.elapsed());
                if since < wait {
                    Next::Done(Reconcile::Backoff {
                        retry_in: wait - since,
                    })
                } else {
                    Next::Restart { ip, port }
                }
            }
        };
        if let Some(p) = reaped {
            let ev = p.terminate(Duration::ZERO).await;
            self.store_last(h, ev).await;
        }
        match next {
            Next::Done(r) => {
                // Out of restarts: nothing is running, so nothing is held.
                if matches!(r, Reconcile::GaveUp { .. }) {
                    self.release_residency(h);
                }
                Ok(r)
            }
            Next::Restart { ip, port } => {
                if port_in_use(ip, port).await {
                    return Err(RuntimeError::Backend(format!(
                        "{ip}:{port} is already taken by something else; not restarting over it"
                    )));
                }
                let mut g = self.instances.lock().await;
                let inst = g.get_mut(&h.instance_id).ok_or_else(|| unknown(h))?;
                let attempt = {
                    let mg = inst.managed.as_mut().ok_or_else(|| unknown(h))?;
                    mg.restarts += 1;
                    mg.restarts
                };
                spawn(self, inst)?;
                Ok(Reconcile::Restarted { attempt })
            }
            Next::Check { client, flavor } => {
                if let Some(a) = self.enforce_loopback(h).await {
                    return Ok(Reconcile::StoppedExposed { reachable_on: a });
                }
                let up = client.probe(flavor).await.health;
                // Healthy for a while, not just once: a server that answers
                // a probe and then dies keeps its restart count, so a crash
                // loop still reaches `GaveUp`.
                if up == Health::Up
                    && let Some(mg) = self
                        .instances
                        .lock()
                        .await
                        .get_mut(&h.instance_id)
                        .and_then(|i| i.managed.as_mut())
                    && mg.started_at.is_some_and(|t| t.elapsed() >= policy.stable_after)
                {
                    mg.restarts = 0;
                }
                Ok(Reconcile::Healthy)
            }
        }
    }
}
