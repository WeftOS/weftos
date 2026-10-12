use super::*;
use crate::project_add_detect::RunKind;
use crate::router_routes::{PortClaim, ProjectInfo, Refused, Route, Source};

const MINE: &str = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1C";

fn det(claims: &[(u16, &str)], routes: Vec<Route>) -> Detected {
    Detected {
        run: RunKind::Unknown,
        pc_http: None,
        claims: claims.iter().map(|(p, u)| PortClaim { port: *p, use_: (*u).into() }).collect(),
        repo_routes: routes,
        env_vars: vec![],
        ports_project: None,
        notes: vec![],
    }
}

fn route(project: &str, prefix: &str, port: u16) -> Route {
    Route { project: project.into(), prefix: prefix.into(), port, ..Default::default() }
}

fn table(routes: Vec<Route>, projects: &[(&str, &str)]) -> RouteTable {
    RouteTable {
        routes,
        refused: vec![],
        projects: projects.iter().map(|(s, u)| ProjectInfo { slug: (*s).into(), ulid: Some((*u).into()), ..Default::default() }).collect(),
    }
}

#[test]
fn one_web_claim_becomes_slash_slug() {
    let d = det(&[(18110, "process-compose-http"), (18120, "web")], vec![]);
    assert_eq!(plan_route(&d, "shasta", MINE, &table(vec![], &[])), RoutePlan::Overlay { prefix: "/shasta".into(), port: 18120 });
}

#[test]
fn repo_routes_win_and_nothing_is_written() {
    let d = det(&[], vec![route("shasta", "/shastaos", 18120)]);
    let t = table(vec![route("shasta", "/shastaos", 18120)], &[("shasta", MINE)]);
    assert_eq!(plan_route(&d, "shasta", MINE, &t), RoutePlan::Repo { prefixes: vec!["/shastaos".into()] });
}

#[test]
fn a_refused_repo_route_is_reported_as_a_conflict() {
    let d = det(&[], vec![route("shasta", "/x", 3000)]);
    let mut t = table(vec![], &[("shasta", MINE)]);
    t.refused.push(Refused { project: "shasta".into(), prefix: "/x".into(), port: 3000, reason: "prefix /x is already routed by project other".into(), source: Source::Repo });
    assert!(matches!(plan_route(&d, "shasta", MINE, &t), RoutePlan::Conflict { reason } if reason.contains("already routed")));
}

#[test]
fn prefix_and_port_taken_by_another_project_refuse() {
    let d = det(&[(3000, "web")], vec![]);
    let other = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1D";
    let t = table(vec![route("other", "/demo", 4000)], &[("other", other)]);
    assert!(matches!(plan_route(&d, "demo", MINE, &t), RoutePlan::Conflict { reason } if reason.contains("prefix /demo")));
    let t = table(vec![route("other", "/o", 3000)], &[("other", other)]);
    assert!(matches!(plan_route(&d, "demo", MINE, &t), RoutePlan::Conflict { reason } if reason.contains("port 3000")));
}

#[test]
fn rerunning_over_its_own_route_is_not_a_conflict() {
    let d = det(&[(3000, "web")], vec![]);
    let t = table(vec![route("demo", "/demo", 3000)], &[("demo", MINE)]);
    assert!(matches!(plan_route(&d, "demo", MINE, &t), RoutePlan::Overlay { .. }));
}

#[test]
fn no_port_or_many_ports_refuse_to_guess() {
    let t = table(vec![], &[]);
    assert!(matches!(plan_route(&det(&[(18110, "process-compose-http")], vec![]), "d", MINE, &t), RoutePlan::None { reason } if reason.contains("no web port")));
    let many = det(&[(3000, "web"), (3001, "api")], vec![]);
    assert!(matches!(plan_route(&many, "d", MINE, &t), RoutePlan::None { reason } if reason.contains("2 ports")));
}

#[test]
fn reserved_slug_prefix_is_a_conflict() {
    let d = det(&[(3000, "web")], vec![]);
    assert!(matches!(plan_route(&d, "api", MINE, &table(vec![], &[])), RoutePlan::Conflict { .. }));
}
