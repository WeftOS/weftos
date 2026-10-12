//! The router route `weaver project add` installs (ADR-116), and the refusals.
//!
//! A project that declares routes in its own `compose/ports.yaml` is left
//! alone: the repository wins and nothing is written. Otherwise the route is
//! `/<slug>` to the project's one non-process-compose port claim, written as a
//! dashboard-style overlay (`~/.weftos/routes/<ULID>.yaml`), never into the
//! checkout. More than one candidate port, no port, a prefix or port another
//! project already routes: reported with the reason, nothing written.

use serde::Serialize;

use crate::project_add_detect::Detected;
use crate::router_routes::{RouteDecl, RouteTable, normalize_prefix};

/// What `add` decided about the route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RoutePlan {
    /// The project's own `compose/ports.yaml` declares these; nothing to write.
    Repo { prefixes: Vec<String> },
    /// Write this overlay route.
    Overlay { prefix: String, port: u16 },
    /// A declared or proposed route cannot be admitted.
    Conflict { reason: String },
    /// No route proposed, and why.
    None { reason: String },
}

fn mine(table: &RouteTable, project: &str, ulid: &str) -> bool {
    table.project(project).is_some_and(|p| p.ulid.as_deref() == Some(ulid))
}

/// Decide the route for project `ulid`/`slug`. `table` is the router's table
/// read after the project was registered (so its own declarations are in it).
pub fn plan_route(det: &Detected, slug: &str, ulid: &str, table: &RouteTable) -> RoutePlan {
    if !det.repo_routes.is_empty() {
        if let Some(x) = table.refused.iter().find(|x| mine(table, &x.project, ulid)) {
            return RoutePlan::Conflict { reason: format!("{} ({}): {}", x.prefix, x.port, x.reason) };
        }
        return RoutePlan::Repo { prefixes: det.repo_routes.iter().map(|r| r.prefix.clone()).collect() };
    }
    let web: Vec<_> = det.claims.iter().filter(|c| !c.use_.starts_with("process-compose")).collect();
    let [claim] = web.as_slice() else {
        let reason = if web.is_empty() {
            "no web port is claimed in compose/ports.yaml; add `routes:` there or set a route from the dashboard".to_owned()
        } else {
            format!(
                "{} ports are claimed ({}); add `routes:` to compose/ports.yaml to say which one is the web front",
                web.len(),
                web.iter().map(|c| format!("{} {}", c.port, c.use_)).collect::<Vec<_>>().join(", ")
            )
        };
        return RoutePlan::None { reason };
    };
    let prefix = match normalize_prefix(&format!("/{slug}")) {
        Ok(p) => p,
        Err(e) => return RoutePlan::Conflict { reason: e },
    };
    if let Some(r) = table.routes.iter().find(|r| r.prefix == prefix && !mine(table, &r.project, ulid)) {
        return RoutePlan::Conflict { reason: format!("prefix {prefix} is already routed by project {}", r.project) };
    }
    if let Some(r) = table.routes.iter().find(|r| r.port == claim.port && !mine(table, &r.project, ulid)) {
        return RoutePlan::Conflict { reason: format!("port {} is already routed by project {} ({})", claim.port, r.project, r.prefix) };
    }
    RoutePlan::Overlay { prefix, port: claim.port }
}

/// The overlay declaration for an `Overlay` plan.
pub fn decl_of(prefix: &str, port: u16) -> RouteDecl {
    RouteDecl { prefix: Some(prefix.to_owned()), port: u64::from(port), ..Default::default() }
}

#[cfg(test)]
#[path = "project_add_route_tests.rs"]
mod tests;
