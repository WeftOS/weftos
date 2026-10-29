//! Role lifecycle for [`crate::infer_wire`]: observing an adopted server,
//! driving a managed one through its governed host, and starting a role's
//! proxy (loopback, or beyond it with a token and a chained permit).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clawft_kernel::infer_proxy::{InferProxy, OccupiedPolicy, PlacementTable, Started, Target};
use clawft_kernel::workload_runtime::infer::Reconcile;
use clawft_kernel::workload_runtime::types::WorkloadRuntime;
use serde_json::{Value, json};
use tracing::warn;

use crate::infer_cfg::Resolved;
use crate::infer_wire::{InferState, InitParts, ProxyState, RoleState, wl_cfg, workload};

/// Wait before a refused or failed managed start is tried again (a refusal
/// is chained each time, so it is not retried every tick).
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Grace given to a server being stopped.
const STOP_GRACE: Duration = Duration::from_secs(5);

fn record(p: &InitParts<'_>, kind: &str, role: &str, why: &str) {
    if let Some(a) = &p.audit {
        a.record(kind, json!({"role": role, "why": why}));
    }
}

/// A role's listeners: the loopback one (token-free, what local consumers
/// use) and, when configured, the exposed one.
pub(crate) struct Proxies {
    pub local: (ProxyState, Option<InferProxy>),
    pub exposed: Option<(ProxyState, Option<InferProxy>)>,
}

/// Start `r`'s proxies. `proxy_port` is always a loopback listener; `expose`
/// adds a second one beyond loopback, with its own pool, behind a token and
/// a chained permit.
pub(crate) async fn start_proxies(r: &Resolved, table: &Arc<PlacementTable>, p: &InitParts<'_>) -> Proxies {
    let Some(port) = r.cfg.proxy_port else {
        return Proxies { local: (ProxyState::Off, None), exposed: None };
    };
    let role = &r.cfg.role;
    let policy = if r.cfg.on_occupied.as_deref() == Some("adopt") { OccupiedPolicy::Adopt } else { OccupiedPolicy::Refuse };
    let refused = |why: String| {
        warn!(role = %role, %why, "inference proxy not started");
        (ProxyState::Refused(why), None)
    };
    let local = match InferProxy::start(
        role,
        ([127, 0, 0, 1], port).into(),
        policy,
        table.clone(),
        p.limits.clone(),
        p.audit.clone(),
    )
    .await
    {
        Ok(Started::Running(px)) => (ProxyState::Listening(px.addr()), Some(px)),
        Ok(Started::Adopted(a)) => (ProxyState::Adopted(a), None),
        Err(e) => refused(e.to_string()),
    };
    let Some(e) = &r.cfg.expose else {
        return Proxies { local, exposed: None };
    };
    // Beyond loopback: a token, then the gate's chained permit.
    let exposed = async {
        let ip: IpAddr = e.listen.parse().map_err(|_| "expose.listen is not an IP address".to_string())?;
        let gate = p.gate.as_ref().ok_or("listening beyond loopback needs the workload governance gate")?;
        let auth = crate::infer_expose::read_token(p.dir, &e.token_file)?;
        let permit = crate::infer_expose::permit_for(gate, role)?;
        InferProxy::start_exposed(
            role,
            SocketAddr::new(ip, e.port),
            OccupiedPolicy::Refuse,
            table.clone(),
            p.limits.clone(),
            p.audit.clone(),
            permit,
            auth,
        )
        .await
        .map_err(|e| e.to_string())
    }
    .await;
    let exposed = match exposed {
        Ok(Started::Running(px)) => (ProxyState::Exposed(px.addr()), Some(px)),
        Ok(Started::Adopted(a)) => (ProxyState::Adopted(a), None),
        Err(why) => {
            record(p, "infer.proxy.expose_refused", role, &why);
            refused(why)
        }
    };
    Proxies { local, exposed: Some(exposed) }
}

impl InferState {
    pub(crate) fn find(&self, role: &str) -> Option<&RoleState> {
        self.roles.iter().find(|r| r.cfg.role == role)
    }

    /// Register an adopted server once it answers.
    async fn observe(&self, r: &RoleState) {
        let mut g = r.handle.lock().await;
        if g.is_some() {
            return;
        }
        let Ok(w) = workload(&r.spec) else { return };
        // `admit` is a read-only probe of the loopback port; a server that
        // is not up yet is retried on the next tick.
        if r.rt.admit(&w).await.is_err() {
            return;
        }
        if let Ok(h) = r.rt.load(&w, &wl_cfg(&self.node_id)).await
            && r.rt.start(&h).await.is_ok()
        {
            *g = Some(h);
        }
    }

    /// A terminal outcome: the role is no longer wanted, and nothing starts
    /// it again until an operator says `infer.start`. (An automatic retry
    /// would loop a crashing server, or put back one that had exposed
    /// itself beyond loopback.)
    fn terminal(&self, r: &RoleState, why: String) {
        let mut g = r.run.lock().unwrap_or_else(|e| e.into_inner());
        g.wanted = false;
        g.started = false;
        g.retry_at = None;
        g.reason = Some(format!("{why}; it stays down until an explicit infer.start"));
    }

    /// Act on what one reconcile pass found for a served managed role.
    pub(crate) async fn on_reconcile(&self, r: &RoleState, out: Option<Reconcile>) {
        if r.managed.is_none() {
            return;
        }
        match out {
            Some(Reconcile::GaveUp { attempts }) => {
                self.terminal(r, format!("the server exited and gave up after {attempts} restarts"));
            }
            Some(Reconcile::StoppedExposed { .. }) => {
                self.terminal(r, "the server was stopped for listening beyond loopback".into());
                // The adapter will not start it again until it is reloaded:
                // drop the instance, so the explicit start loads a new one.
                if let (Some(m), Some(h)) = (&r.managed, r.handle.lock().await.take()) {
                    let _ = m.host.unload(h).await;
                }
            }
            _ => {}
        }
    }

    fn fail(&self, r: &RoleState, why: String) {
        let mut g = r.run.lock().unwrap_or_else(|e| e.into_inner());
        g.reason = Some(why);
        g.started = false;
        g.retry_at = Some(Instant::now() + RETRY_AFTER);
    }

    /// Bring a wanted managed role up: load, then start, each through the
    /// host (gated and chained). A refusal, including an unplaceable start
    /// (memory budget or co-residency), is recorded as the role's reason
    /// and retried after a pause.
    async fn drive(&self, r: &RoleState) {
        let Some(m) = &r.managed else { return };
        {
            let g = r.run.lock().unwrap_or_else(|e| e.into_inner());
            if !g.wanted || g.retry_at.is_some_and(|t| Instant::now() < t) {
                return;
            }
        }
        let mut h = r.handle.lock().await;
        let w = match workload(&r.spec) {
            Ok(w) => w,
            Err(e) => return self.fail(r, e),
        };
        if h.is_none() {
            match m.host.load(&w, &wl_cfg(&self.node_id)).await {
                Ok(x) => *h = Some(x),
                Err(e) => return self.fail(r, e.to_string()),
            }
        }
        let started = r.run.lock().unwrap_or_else(|e| e.into_inner()).started;
        if started {
            // A started instance can still fail to load (Ollama): notice it,
            // so the role stops claiming to run.
            if let Some(handle) = h.as_ref() {
                let st = m.host.status(handle).await;
                if let Some(d) = st.detail.filter(|d| d.starts_with("load failed")) {
                    return self.fail(r, d);
                }
            }
            return;
        }
        if !started && let Some(handle) = h.as_ref() {
            match m.host.start(handle).await {
                Ok(()) => {
                    let mut g = r.run.lock().unwrap_or_else(|e| e.into_inner());
                    g.started = true;
                    g.reason = None;
                    g.retry_at = None;
                }
                Err(e) => self.fail(r, e.to_string()),
            }
        }
    }

    /// One local pass: bring wanted managed roles up, (re)register each
    /// served instance and update the table.
    pub async fn sync_once(&self) {
        self.sync_svc();
        for r in self.roles.iter().filter(|r| r.cfg.svc.is_none()) {
            if r.managed.is_some() {
                self.drive(r).await;
            } else {
                self.observe(r).await;
            }
            let h = r.handle.lock().await.clone();
            let live = r.managed.is_none() || r.run.lock().unwrap_or_else(|e| e.into_inner()).started;
            match h {
                Some(h) if live => {
                    let out = self.table.sync_local(&r.cfg.role, &r.rt, &h).await;
                    self.on_reconcile(r, out.reconcile).await;
                }
                _ => self.table.deregister_local(&r.cfg.role),
            }
        }
    }

    /// `infer.start`: want a managed role up and try now.
    pub async fn start_role(&self, role: &str) -> Result<Value, String> {
        let r = self.find(role).ok_or_else(|| format!("unknown role {role:?}"))?;
        if r.managed.is_none() {
            let why = r.run.lock().unwrap_or_else(|e| e.into_inner()).reason.clone();
            return Err(why.unwrap_or_else(|| format!("{role} is adopted: WeftOS observes it and never starts it")));
        }
        {
            let mut g = r.run.lock().unwrap_or_else(|e| e.into_inner());
            g.wanted = true;
            g.retry_at = None;
        }
        // An explicit start is the only thing that gives a server that gave
        // up a fresh restart budget.
        if let Some(h) = r.handle.lock().await.clone() {
            r.rt.reset_restarts(&h).await;
        }
        self.drive(r).await;
        Ok(self.role_json(r))
    }

    /// `infer.stop`: stop and unload a managed role (the server this
    /// adapter started, and nothing else).
    pub async fn stop_role(&self, role: &str) -> Result<Value, String> {
        let r = self.find(role).ok_or_else(|| format!("unknown role {role:?}"))?;
        let Some(m) = &r.managed else {
            return Err(format!("{role} is adopted: WeftOS never stops a server it did not start"));
        };
        {
            let mut g = r.run.lock().unwrap_or_else(|e| e.into_inner());
            g.wanted = false;
            g.started = false;
            g.reason = None;
        }
        self.table.deregister_local(role);
        let h = r.handle.lock().await.take();
        if let Some(h) = h {
            let stop = m.host.stop(&h, STOP_GRACE).await;
            let unload = m.host.unload(h).await;
            if let Err(e) = stop.map(|_| ()).and(unload) {
                return Err(format!("stopping {role}: {e}"));
            }
        }
        Ok(self.role_json(r))
    }

    /// Stop every managed role and the sync loop (daemon shutdown).
    pub async fn shutdown(&self) {
        self.stopping.store(true, std::sync::atomic::Ordering::Relaxed);
        for r in self.roles.iter().filter(|r| r.managed.is_some()) {
            let _ = self.stop_role(&r.cfg.role).await;
        }
    }

    /// One role as `infer.status` reports it.
    pub(crate) fn role_json(&self, r: &RoleState) -> Value {
        let serves = match self.table.resolve(&r.cfg.role) {
            Some(Target::Local { .. }) => json!("local"),
            Some(Target::Remote { node_id }) => json!({"remote": node_id}),
            Some(Target::Svc { topic, .. }) => json!({"svc": topic}),
            None => Value::Null,
        };
        let (wanted, started, reason) = {
            let g = r.run.lock().unwrap_or_else(|e| e.into_inner());
            (g.wanted, g.started, g.reason.clone())
        };
        let managed = r.managed.is_some() || r.cfg.mode.as_deref() == Some("managed");
        let svc = r.cfg.svc.as_ref();
        let state = if svc.is_some() {
            if serves.is_null() { "waiting" } else { "serving" }
        } else if managed {
            match (&reason, started) {
                (Some(w), _) if w.contains("unplaceable") => "unplaceable",
                (Some(w), _) if w.starts_with("not available") => "unavailable",
                (Some(_), _) => "refused",
                (None, true) if matches!(serves, Value::String(_)) => "running",
                (None, true) => "starting",
                (None, false) if wanted => "starting",
                (None, false) => "stopped",
            }
        } else if matches!(serves, Value::String(_)) {
            "observing"
        } else {
            "waiting"
        };
        json!({
            "role": r.cfg.role,
            "mode": if svc.is_some() { "svc" } else if managed { "managed" } else { "adopted" },
            "svc": svc.map(|s| json!({"topic": s.topic, "model": s.model})),
            "flavor": if svc.is_some() { "svc" } else { r.spec.runtime.id() },
            "model": r.spec.model,
            "instance_port": r.spec.serve.port,
            "memory_gb": r.spec.memory.weights_bytes as f64 / 1e9,
            "latency_class": r.spec.latency_class,
            "sticky": r.spec.sticky,
            "state": state,
            "reason": reason,
            "proxy": &*r.proxy.lock().unwrap_or_else(|e| e.into_inner()),
            "exposed_proxy": r.exposed_proxy.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            "serves": serves,
            "exposed": self.table.exposed_roles().contains(&r.cfg.role),
        })
    }
}
