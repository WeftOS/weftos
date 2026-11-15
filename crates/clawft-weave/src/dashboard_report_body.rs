//! The heartbeat report body builder (split out of `dashboard_report`; no behaviour change).

use serde_json::{Map, Value, json};

use super::{Dashboard, unit_key, unit_state};
use crate::dashboard_budget;
use crate::dashboard_workspaces;
use crate::mesh_pair_requests;

impl Dashboard {
    /// The heartbeat request body (what `heartbeat.sh` sent).
    pub async fn build_body(&self) -> Value {
        let version = env!("CARGO_PKG_VERSION");
        let mut unit_facts = Map::new();
        for u in &self.cfg.units {
            unit_facts.insert(unit_key(u), Value::String(unit_state(u).await));
        }
        let mut observed = Map::new();
        for (ulid, child) in self.children.children().await {
            let mut o = Map::new();
            o.insert("release".into(), json!(format!("v{version}")));
            o.extend(unit_facts.clone());
            o.insert("project_child_state".into(), json!(child));
            if let Some(m) = &self.ambient.mesh_listen {
                o.insert("mesh_listen".into(), json!(m));
            }
            if let Some(g) = &self.ambient.gateway_url {
                o.insert("gateway_url".into(), json!(g));
            }
            observed.insert(ulid, Value::Object(o));
        }
        let mut report = json!({
            "host": self.cfg.installation_id, "weaver_version": version, "kind": self.cfg.kind, "observed": observed,
        });
        // The daemon installs its registry after the reporter starts; pick it up on the first beat after.
        if self.instances.get().is_none()
            && let Some(reg) = crate::instance_registry::global()
        {
            self.set_instance_registry(reg);
        }
        if let Some(reg) = self.instances.get() {
            reg.sync().await;
            report["instances"] = reg.summary();
        }
        // ADR-120: what this node has and what it offers (compact; full logs stay on the node).
        if let Some(resources) = crate::expose_rt::resources_json() {
            report["resources"] = resources;
        }
        if self.exposures.get().is_none()
            && let Some(reg) = crate::expose_rt::global()
        {
            self.set_exposure_registry(reg);
        }
        if let Some(reg) = self.exposures.get() {
            report["exposures"] = reg.report(clawft_kernel::exposure::registry::now_ms());
        }
        #[cfg(all(feature = "placement", unix))]
        crate::cogbus_serve::attach_report(&mut report); // ADR-122 B3: `topics`
        // Routes go in before workspaces, whose size trimming measures the whole report.
        if let Some(src) = self.routes.get()
            && let Some(routes) = src.routes().await
        {
            report["routes"] = routes;
        }
        if let Some(src) = self.services.get() {
            let services = src.services().await;
            if !services.is_empty() {
                report["services"] = json!(services);
            }
        }
        if let (true, Some(src)) = (self.cfg.report_workspaces, &self.workspaces) {
            dashboard_workspaces::attach(&mut report, src.workspaces().await);
        }
        if let Some(src) = self.pair.get() {
            if let Some(id) = src.mesh_identity().await {
                report["mesh_identity"] = json!(id);
            }
            mesh_pair_requests::attach(&mut report, &src.pair_requests().await);
        }
        let (mesh_id, rt) = self.local_mesh_id();
        crate::mesh_genesis::attach(&mut report, mesh_id, rt.as_ref().map(|r| r.dir.as_path()));
        dashboard_budget::fit(&mut report, dashboard_budget::REPORT_BUDGET_BYTES);
        json!({
            "node_id": self.cfg.node_id,
            "installation_id": self.cfg.installation_id,
            "report": report,
        })
    }
}
