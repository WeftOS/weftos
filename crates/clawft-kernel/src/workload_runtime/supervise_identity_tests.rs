//! The uid/gid/groups a launched cog switches to (ADR-122 gap 2). Nothing
//! here calls `setuid`: the plan is what the child would apply, checked as
//! data, and the refusals happen before any fork.

use std::net::SocketAddr;

use super::*;
use crate::workload_runtime::host_contract::{CogIdentity, HostContract};
use crate::workload_runtime::native::effective_identity;

fn spec(run_as: Option<(u32, u32)>, groups: Vec<u32>) -> LaunchSpec {
    LaunchSpec {
        program: "/bin/true".into(),
        args: vec![],
        env: vec![],
        cwd: "/".into(),
        limits: ProcLimits::for_ram_mb(64),
        output_limit: 1024,
        run_as,
        groups,
    }
}

fn contract() -> HostContract {
    HostContract::new(SocketAddr::from(([127, 0, 0, 1], 5006)))
}

#[test]
fn the_child_sets_groups_then_gid_then_uid_from_the_spec() {
    let plan = spec(Some((60007, 4321)), vec![4321]).child_plan();
    assert_eq!(plan.drop, Some((60007, 4321, vec![4321])));
    assert_eq!(plan.limits, ProcLimits::for_ram_mb(64));
}

#[test]
fn no_run_as_means_no_switch_and_groups_alone_do_nothing() {
    assert_eq!(spec(None, vec![]).child_plan().drop, None);
    assert_eq!(spec(None, vec![4321]).child_plan().drop, None, "groups only ride with a uid");
}

#[test]
fn an_allocated_identity_overrides_the_adapter_run_as() {
    let id = CogIdentity { uid: 60001, gid: 4321, groups: vec![4321] };
    let c = contract().with_run_as(id);
    assert_eq!(effective_identity(Some((999, 999)), &c), (Some((60001, 4321)), vec![4321]));
    assert_eq!(effective_identity(Some((999, 999)), &contract()), (Some((999, 999)), vec![]));
    assert_eq!(effective_identity(None, &contract()), (None, vec![]));
}

#[test]
fn a_launch_as_uid_zero_is_refused_before_any_fork() {
    let e = Supervised::spawn(spec(Some((0, 0)), vec![])).err().expect("refused");
    assert!(matches!(e, RuntimeError::InvalidConfig(m) if m.contains("not unprivileged")));
}

#[test]
fn a_root_host_without_any_identity_is_refused() {
    if !running_as_root() {
        eprintln!("skipped: needs a root test process to reach the refusal");
        return;
    }
    let e = Supervised::spawn(spec(None, vec![])).err().expect("refused");
    assert!(matches!(e, RuntimeError::AdmissionRefused(_)));
}
