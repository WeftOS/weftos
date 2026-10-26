//! [`WorkloadHost`]: gates every adapter transition through governance and
//! chains what the adapter did (ADR-099 sections 4 and 5).
//!
//! The gate chains its own decision under the action's kind with source
//! `workload`; the host then chains the transition's outcome under the same
//! kind with source [`RUNTIME_CHAIN_SOURCE`]. Admission refusals and adapter
//! failures are chained as `workload.refuse`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::cog_spec::valid_value;
use super::evidence::RunEvidence;
use super::types::{
    ControlMode, InstanceHandle, InstanceStatus, Preemption, RuntimeError, VerifiedWorkload, WorkloadConfig,
    WorkloadRuntime, WorkloadSource,
};
use crate::chain::{self, ChainManager};
use crate::gate::{GateBackend, GateDecision};
use crate::workload_governance::{NetworkPolicy, NodeTrustTier};

/// Chain source for adapter lifecycle outcomes.
pub const RUNTIME_CHAIN_SOURCE: &str = "workload.runtime";

/// Governs and records one adapter.
pub struct WorkloadHost {
    runtime: Arc<dyn WorkloadRuntime>,
    gate: Arc<dyn GateBackend>,
    chain: Option<Arc<ChainManager>>,
    agent_id: String,
    /// Live: the control plane lowers it when the operator demotes the node.
    node_tier: std::sync::RwLock<NodeTrustTier>,
    loaded: Mutex<HashMap<String, (VerifiedWorkload, bool)>>,
}

impl WorkloadHost {
    /// Host for `runtime` on a node of `node_tier`.
    pub fn new(
        runtime: Arc<dyn WorkloadRuntime>,
        gate: Arc<dyn GateBackend>,
        agent_id: impl Into<String>,
        node_tier: NodeTrustTier,
    ) -> Self {
        Self {
            runtime,
            gate,
            chain: None,
            agent_id: agent_id.into(),
            node_tier: std::sync::RwLock::new(node_tier),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// Attach the chain.
    pub fn with_chain(mut self, cm: Arc<ChainManager>) -> Self {
        self.chain = Some(cm);
        self
    }

    /// The trust tier this host's node is governed at right now.
    pub fn node_tier(&self) -> NodeTrustTier {
        self.node_tier
            .read()
            .map_or(NodeTrustTier::Discovered, |t| *t)
    }

    /// Change the governed tier (an operator demotion or promotion). Takes
    /// effect on the next gated call, including on instances already placed.
    pub fn set_node_tier(&self, tier: NodeTrustTier) {
        if let Ok(mut t) = self.node_tier.write() {
            *t = tier;
        }
    }

    /// The adapter.
    pub fn runtime(&self) -> &Arc<dyn WorkloadRuntime> {
        &self.runtime
    }

    /// Governance context. `network` is what the adapter actually gives
    /// instances ([`WorkloadRuntime::network_exposure`]). `secrets` is
    /// false because no adapter delivers operator secrets: the only
    /// credential an instance gets is its own `COGNITUM_COG_TOKEN`, minted
    /// here for that instance's ingest bridge (ADR-100 section 4).
    fn context(&self, w: &VerifiedWorkload, kind: &str, emulated: bool) -> Value {
        let network: NetworkPolicy = self.runtime.network_exposure();
        let (package_id, keys, hashes) = w.revocation_refs();
        let (trust, cost) = match &w.source {
            WorkloadSource::SignedPackage(p) => (
                "pinned_signer",
                (f64::from(p.spec.resources.cpu_pct) / 400.0).clamp(0.0, 1.0),
            ),
            WorkloadSource::StorePin { .. } => ("operator_attested", 0.25),
            // `PackageTrust::ProjectCert`: matched only by a permit that
            // names it (the supervisor's, see `project_supervisor_permit`).
            WorkloadSource::Project(_) => ("project_cert", 0.25),
            // Managed: model weights are operator-attested hash manifests
            // (ADR-101 section 3). Adopted: a server the operator started
            // is only observed, nothing is verified. Cost is the share of a
            // 128 GiB unified pool.
            WorkloadSource::Inference(p) => {
                let m = &p.spec.memory;
                let bytes = m.weights_bytes.saturating_add(m.kv_budget_bytes);
                let trust = if self.runtime.control_mode() == ControlMode::Adopted {
                    "adopted_unverified"
                } else {
                    "operator_attested"
                };
                (
                    trust,
                    (bytes as f64 / (128.0 * 1024.0 * 1024.0 * 1024.0)).clamp(0.1, 1.0),
                )
            }
        };
        json!({ "workload": {
            "kind": kind,
            "package_trust": trust,
            "node_tier": self.node_tier(),
            "network": network,
            "secrets": false,
            "emulated": emulated,
            "resource_cost": cost,
            "package_id": package_id,
            "signer_keys": keys,
            "artifact_hashes": hashes,
        }})
    }

    pub(super) fn check(
        &self,
        action: &str,
        w: &VerifiedWorkload,
        kind: &str,
        emulated: bool,
    ) -> Result<(), RuntimeError> {
        self.verdict(action, w, kind, emulated, false)
    }

    /// [`Self::check`] for taking down what is already placed (stop,
    /// unload, re-adoption): a denial caused by the node tier alone is
    /// waived by the gate ([`GateBackend::check_teardown`]).
    fn check_teardown(
        &self,
        action: &str,
        w: &VerifiedWorkload,
        kind: &str,
        emulated: bool,
    ) -> Result<(), RuntimeError> {
        self.verdict(action, w, kind, emulated, true)
    }

    fn verdict(
        &self,
        action: &str,
        w: &VerifiedWorkload,
        kind: &str,
        emulated: bool,
        teardown: bool,
    ) -> Result<(), RuntimeError> {
        let ctx = self.context(w, kind, emulated);
        let d = if teardown {
            self.gate.check_teardown(&self.agent_id, action, &ctx)
        } else {
            self.gate.check(&self.agent_id, action, &ctx)
        };
        match d {
            GateDecision::Permit { .. } => Ok(()),
            GateDecision::Deny { reason, .. } => Err(RuntimeError::Governance(format!(
                "{action} denied: {reason}"
            ))),
            GateDecision::Defer { reason } => Err(RuntimeError::Governance(format!(
                "{action} deferred to a human: {reason}"
            ))),
        }
    }

    fn record(&self, kind: &str, payload: Value) {
        if let Some(cm) = &self.chain {
            cm.append(RUNTIME_CHAIN_SOURCE, kind, Some(payload));
        }
    }

    fn base(&self, w: &VerifiedWorkload, instance: Option<&str>) -> Value {
        json!({
            "runtime": self.runtime.id(),
            "workload_id": w.id,
            "version": w.version,
            "instance_id": instance,
        })
    }

    pub(super) fn outcome<T>(&self, kind: &str, mut payload: Value, r: &Result<T, RuntimeError>) {
        match r {
            Ok(_) => {
                payload["outcome"] = json!("ok");
                self.record(kind, payload);
            }
            Err(e) => {
                payload["outcome"] = json!("error");
                payload["failed_action"] = json!(kind);
                payload["error_code"] = json!(e.code());
                payload["reason"] = json!(e.to_string());
                self.record(chain::EVENT_KIND_WORKLOAD_REFUSE, payload);
            }
        }
    }

    async fn workload_of(
        &self,
        h: &InstanceHandle,
    ) -> Result<(VerifiedWorkload, bool), RuntimeError> {
        self.loaded
            .lock()
            .await
            .get(&h.instance_id)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownInstance(h.instance_id.clone()))
    }

    /// Gate, admit and load. Store pins are also gated as `workload.install`.
    pub async fn load(
        &self,
        w: &VerifiedWorkload,
        cfg: &WorkloadConfig,
    ) -> Result<InstanceHandle, RuntimeError> {
        let admission = self.runtime.admit(w).await;
        let emulated = admission.as_ref().is_ok_and(|a| a.emulated);
        let mut payload = self.base(w, None);
        payload["admission"] = json!(admission.as_ref().ok());
        payload["host_contract"] = cfg.host.audit();
        if let Err(e) = &admission {
            self.outcome::<()>(chain::EVENT_KIND_WORKLOAD_LOAD, payload, &Err(e.clone()));
            return Err(e.clone());
        }
        let store = matches!(w.source, WorkloadSource::StorePin { .. });
        if store {
            self.check("workload.install", w, &w.kind, emulated)?;
        }
        self.check("workload.load", w, &w.kind, emulated)?;
        let r = self.runtime.load(w, cfg).await;
        if let Ok(h) = &r {
            payload["instance_id"] = json!(h.instance_id);
            self.loaded
                .lock()
                .await
                .insert(h.instance_id.clone(), (w.clone(), emulated));
            // Chained only when something was actually installed.
            if h.store_installed {
                self.record(
                    chain::EVENT_KIND_WORKLOAD_INSTALL,
                    json!({
                        "runtime": self.runtime.id(), "workload_id": w.id, "version": w.version,
                        "instance_id": h.instance_id, "outcome": "ok", "source": "store_pin",
                    }),
                );
            }
        }
        if let Err(RuntimeError::StrandedInstall {
            handle,
            rolled_back,
            ..
        }) = &r
        {
            // The device store changed even though the load failed.
            self.record(
                chain::EVENT_KIND_WORKLOAD_INSTALL,
                json!({
                    "runtime": self.runtime.id(), "workload_id": w.id, "version": w.version,
                    "instance_id": handle.instance_id, "source": "store_pin",
                    "outcome": if *rolled_back { "rolled-back" } else { "stranded" },
                }),
            );
            if !rolled_back {
                // Still on the device: keep it unloadable through the host.
                self.loaded
                    .lock()
                    .await
                    .insert(handle.instance_id.clone(), (w.clone(), emulated));
            }
        }
        self.outcome(chain::EVENT_KIND_WORKLOAD_LOAD, payload, &r);
        r
    }

    /// Re-attach an instance loaded before a controller restart
    /// ([`WorkloadRuntime::adopt`]), gated as `workload.load` and chained
    /// with phase `readopted`. A no-op if the instance is already known.
    pub async fn adopt(
        &self,
        h: &InstanceHandle,
        w: &VerifiedWorkload,
    ) -> Result<(), RuntimeError> {
        if self.loaded.lock().await.contains_key(&h.instance_id) {
            return Ok(());
        }
        self.check_teardown("workload.load", w, &w.kind, false)?;
        let r = self.runtime.adopt(h, w).await;
        if r.is_ok() {
            self.loaded
                .lock()
                .await
                .insert(h.instance_id.clone(), (w.clone(), false));
        }
        let mut payload = self.base(w, Some(&h.instance_id));
        payload["phase"] = json!("readopted");
        self.outcome(chain::EVENT_KIND_WORKLOAD_LOAD, payload, &r);
        r
    }

    /// Gate and start.
    pub async fn start(&self, h: &InstanceHandle) -> Result<(), RuntimeError> {
        let (w, emu) = self.workload_of(h).await?;
        self.check("workload.start", &w, &w.kind, emu)?;
        let r = self.runtime.start(h).await;
        self.outcome(
            chain::EVENT_KIND_WORKLOAD_START,
            self.base(&w, Some(&h.instance_id)),
            &r,
        );
        r
    }

    /// Gate and run one console cycle, chained as `workload.start` with
    /// phase `console`.
    ///
    /// Instances the adapter says must stop first (sensor feed contention,
    /// including ones WeftOS did not load) are each gated as
    /// `workload.stop`; if any is denied nothing is stopped. Each stop is
    /// chained as it happens (so a failed console run still leaves the
    /// record), and afterwards each stopped instance is restarted, gated
    /// as `workload.start` and chained with phase `resume-after-console`.
    pub async fn console(
        &self,
        h: &InstanceHandle,
        command: &str,
    ) -> Result<RunEvidence, RuntimeError> {
        let (w, emu) = self.workload_of(h).await?;
        self.check("workload.start", &w, &w.kind, emu)?;
        let mut payload = self.base(&w, Some(&h.instance_id));
        payload["phase"] = json!("console");
        let pre = match self.preemptions_permitted(h).await {
            Ok(p) => p,
            Err(e) => {
                self.outcome::<()>(chain::EVENT_KIND_WORKLOAD_START, payload, &Err(e.clone()));
                return Err(e);
            }
        };
        let mut stopped = Vec::new();
        let mut failed = None;
        for (p, _) in &pre {
            let r = self.runtime.preempt(p).await;
            self.outcome(
                chain::EVENT_KIND_WORKLOAD_STOP,
                self.preempt_payload(p, h, "preempt-for-console"),
                &r,
            );
            match r {
                Ok(()) => stopped.push(p),
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        let mut r = match failed {
            Some(e) => Err(e),
            None => self.runtime.console(h, command).await,
        };
        let mut resumed = Vec::new();
        for (p, pw) in pre.iter().filter(|(p, _)| stopped.contains(&p)) {
            if self.check("workload.start", pw, &pw.kind, false).is_err() {
                continue; // the gate chained its denial; the cog stays stopped
            }
            let rr = self.runtime.resume(p).await;
            self.outcome(
                chain::EVENT_KIND_WORKLOAD_START,
                self.preempt_payload(p, h, "resume-after-console"),
                &rr,
            );
            if rr.is_ok() {
                resumed.push(p.workload_id.clone());
            }
        }
        let ids: Vec<String> = stopped.iter().map(|p| p.workload_id.clone()).collect();
        payload["preempted"] = json!(ids);
        payload["resumed"] = json!(resumed);
        if let Ok(ev) = &mut r {
            ev.stopped_for_console = ids;
            payload["evidence"] = ev.audit();
        }
        self.outcome(chain::EVENT_KIND_WORKLOAD_START, payload, &r);
        r
    }

    /// The adapter's console preemptions, each gated as `workload.stop`.
    async fn preemptions_permitted(
        &self,
        h: &InstanceHandle,
    ) -> Result<Vec<(Preemption, VerifiedWorkload)>, RuntimeError> {
        let mut out = Vec::new();
        for p in self.runtime.console_preemptions(h).await? {
            let version = if valid_value(&p.version) {
                p.version.as_str()
            } else {
                "unknown"
            };
            let pw = VerifiedWorkload::store_pin(&p.registry, &p.workload_id, version, None)?;
            self.check("workload.stop", &pw, &pw.kind, false)
                .map_err(|e| {
                    RuntimeError::Governance(format!(
                        "console run needs {} stopped: {e}",
                        p.workload_id
                    ))
                })?;
            out.push((p, pw));
        }
        Ok(out)
    }

    fn preempt_payload(&self, p: &Preemption, h: &InstanceHandle, phase: &str) -> Value {
        json!({
            "runtime": self.runtime.id(),
            "workload_id": p.workload_id,
            "version": p.version,
            "phase": phase,
            "for_instance": h.instance_id,
        })
    }

    /// Gate and stop.
    pub async fn stop(
        &self,
        h: &InstanceHandle,
        grace: Duration,
    ) -> Result<RunEvidence, RuntimeError> {
        self.stop_because(h, grace, None).await
    }

    /// [`stop`](Self::stop), with the chained `workload.stop` event naming
    /// what asked for it (`cause`, e.g. the cogbus after repeated refusals).
    pub async fn stop_because(
        &self,
        h: &InstanceHandle,
        grace: Duration,
        cause: Option<Value>,
    ) -> Result<RunEvidence, RuntimeError> {
        let (w, emu) = self.workload_of(h).await?;
        self.check_teardown("workload.stop", &w, &w.kind, emu)?;
        let r = self.runtime.stop(h, grace).await;
        let mut payload = self.base(&w, Some(&h.instance_id));
        if let Some(c) = cause {
            payload["cause"] = c;
        }
        if let Ok(ev) = &r {
            payload["evidence"] = ev.audit();
        }
        self.outcome(chain::EVENT_KIND_WORKLOAD_STOP, payload, &r);
        r
    }

    /// Gate and unload.
    pub async fn unload(&self, h: InstanceHandle) -> Result<(), RuntimeError> {
        let (w, emu) = self.workload_of(&h).await?;
        self.check_teardown("workload.unload", &w, &w.kind, emu)?;
        let payload = self.base(&w, Some(&h.instance_id));
        let iid = h.instance_id.clone();
        let r = self.runtime.unload(h).await;
        if r.is_ok() {
            self.loaded.lock().await.remove(&iid);
        }
        self.outcome(chain::EVENT_KIND_WORKLOAD_UNLOAD, payload, &r);
        r
    }

    /// The workload a loaded instance came from, if this host loaded or
    /// adopted it.
    pub async fn workload_for(&self, h: &InstanceHandle) -> Option<VerifiedWorkload> {
        self.loaded
            .lock()
            .await
            .get(&h.instance_id)
            .map(|(w, _)| w.clone())
    }

    /// Stop and unload an instance because its package, signer or artifact
    /// was revoked. Not gated: the applied revocation is the authority
    /// (`why` names it in both records), so enforcement cannot depend on an
    /// operator having written a stop or unload permit. The unload is
    /// attempted even when the stop fails (the instance may already have
    /// exited). Each step is chained like the gated one, with
    /// `forced_by_revocation` set.
    pub async fn revoke_teardown(
        &self,
        h: &InstanceHandle,
        grace: Duration,
        why: Value,
    ) -> Result<(), RuntimeError> {
        let (w, _emu) = self.workload_of(h).await?;
        let mut payload = self.base(&w, Some(&h.instance_id));
        payload["forced_by_revocation"] = why;
        let stopped = self.runtime.stop(h, grace).await;
        let mut stop_payload = payload.clone();
        if let Ok(ev) = &stopped {
            stop_payload["evidence"] = ev.audit();
        }
        self.outcome(chain::EVENT_KIND_WORKLOAD_STOP, stop_payload, &stopped);
        let unloaded = self.runtime.unload(h.clone()).await;
        if unloaded.is_ok() {
            self.loaded.lock().await.remove(&h.instance_id);
        }
        self.outcome(chain::EVENT_KIND_WORKLOAD_UNLOAD, payload, &unloaded);
        unloaded
    }

    /// Status (not gated: read-only).
    pub async fn status(&self, h: &InstanceHandle) -> InstanceStatus {
        self.runtime.status(h).await
    }
}
