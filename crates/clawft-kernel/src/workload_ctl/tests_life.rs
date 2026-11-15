//! The instance state machine, the bounded-restart policy and the kinds'
//! health definitions (pure).

use super::lifecycle::LifecycleState::{self, *};
use super::lifecycle::{
    InstanceLife, LifecyclePolicy, RestartDecision, RestartPolicy, can_transition,
};
use crate::workload_kind::{
    Health, HealthSample, KindRegistry, SampleState, judge_process,
};

const ALL: [LifecycleState; 16] = [
    Requested, Placed, Fetching, Verified, Loaded, Running, Unhealthy, Restarting, Stopping,
    Stopped, Finished, Failed, Lost, Rescheduled, Revoked, Unloaded,
];

#[test]
fn the_adr_path_is_legal_end_to_end() {
    let path = [Requested, Placed, Fetching, Verified, Loaded, Running, Stopping, Unloaded];
    for w in path.windows(2) {
        assert!(can_transition(w[0], w[1]), "{:?} -> {:?}", w[0], w[1]);
    }
}

#[test]
fn health_and_restart_steps_are_legal_and_nonsense_is_not() {
    for (a, b) in [
        (Running, Unhealthy),
        (Unhealthy, Restarting),
        (Restarting, Running),
        (Restarting, Unhealthy),
        (Unhealthy, Running),
        (Unhealthy, Failed),
        (Running, Finished),
        (Stopped, Running),
        (Lost, Rescheduled),
        (Lost, Running),
        (Rescheduled, Unloaded),
    ] {
        assert!(can_transition(a, b), "{a:?} -> {b:?}");
    }
    for (a, b) in [
        (Running, Loaded),
        (Running, Placed),
        (Stopped, Restarting),
        (Finished, Restarting),
        (Rescheduled, Running),
        (Rescheduled, Lost),
        (Rescheduled, Revoked),
        (Revoked, Running),
        (Revoked, Lost),
        (Unloaded, Running),
        (Running, Running),
    ] {
        assert!(!can_transition(a, b), "{a:?} -> {b:?} must be refused");
    }
}

#[test]
fn a_revocation_or_a_lost_node_can_interrupt_anything_that_exists() {
    for s in ALL {
        let revocable = !matches!(s, Unloaded | Revoked | Rescheduled);
        assert_eq!(can_transition(s, Revoked), revocable, "{s:?} -> Revoked");
        let losable = !matches!(s, Unloaded | Rescheduled | Lost | Revoked);
        assert_eq!(can_transition(s, Lost), losable, "{s:?} -> Lost");
    }
}

#[test]
fn nothing_leaves_unloaded() {
    for s in ALL {
        assert!(!can_transition(Unloaded, s));
    }
}

#[test]
fn an_illegal_step_is_refused_and_changes_nothing() {
    let mut l = InstanceLife::new(Running, 5);
    let e = l.transition(Loaded, 9).unwrap_err();
    assert_eq!((e.from, e.to), (Running, Loaded));
    assert_eq!((l.state, l.since_ms), (Running, 5));
    assert_eq!(l.transition(Unhealthy, 9).unwrap(), Running);
    assert_eq!((l.state, l.since_ms), (Unhealthy, 9));
}

#[test]
fn entering_running_clears_the_miss_count() {
    let mut l = InstanceLife::new(Running, 0);
    l.misses = 2;
    l.transition(Unhealthy, 1).unwrap();
    assert_eq!(l.misses, 2);
    l.transition(Running, 2).unwrap();
    assert_eq!(l.misses, 0);
}

fn policy() -> RestartPolicy {
    RestartPolicy {
        max_restarts: 3,
        window_ms: 10_000,
        backoff_base_ms: 100,
        backoff_max_ms: 250,
    }
}

#[test]
fn restarts_back_off_exponentially_up_to_the_cap() {
    let p = policy();
    let mut l = InstanceLife::new(Unhealthy, 0);
    assert_eq!(l.restart_decision(&p, 0), RestartDecision::Restart { attempt: 1 });
    l.record_restart(&p, 0);
    // The second waits the base (100), the third twice that, capped at 250.
    assert_eq!(l.restart_decision(&p, 50), RestartDecision::Wait { until_ms: 100 });
    assert_eq!(l.restart_decision(&p, 100), RestartDecision::Restart { attempt: 2 });
    l.record_restart(&p, 100);
    assert_eq!(l.restart_decision(&p, 150), RestartDecision::Wait { until_ms: 300 });
    l.record_restart(&p, 300);
    assert_eq!(l.restart_decision(&p, 310), RestartDecision::Exhausted);
    // A fourth would wait 250 (the cap), not 400, had the budget allowed it.
    let p4 = RestartPolicy { max_restarts: 4, ..p };
    assert_eq!(l.restart_decision(&p4, 310), RestartDecision::Wait { until_ms: 550 });
}

#[test]
fn restarts_outside_the_window_are_forgotten() {
    let p = policy();
    let mut l = InstanceLife::new(Unhealthy, 0);
    for t in [0, 1_000, 2_000] {
        l.record_restart(&p, t);
    }
    assert_eq!(l.restart_decision(&p, 3_000), RestartDecision::Exhausted);
    // Ten seconds after the first, only two still count.
    assert!(matches!(
        l.restart_decision(&p, 10_500),
        RestartDecision::Restart { attempt: 3 }
    ));
    l.record_restart(&p, 10_500);
    assert_eq!(l.restarts.len(), 3, "the first one aged out and the new one was added");
}

#[test]
fn zero_max_restarts_means_never_restart() {
    let p = RestartPolicy {
        max_restarts: 0,
        ..policy()
    };
    assert_eq!(
        InstanceLife::new(Unhealthy, 0).restart_decision(&p, 0),
        RestartDecision::Exhausted
    );
}

fn sample(state: SampleState, exit_code: Option<i32>, continuous: bool) -> HealthSample {
    HealthSample {
        state,
        exit_code,
        continuous,
        heartbeat_age_ms: None,
    }
}

#[test]
fn process_health_is_judged_by_state_and_exit() {
    use SampleState::*;
    assert_eq!(judge_process(&sample(Running, None, true)), Health::Healthy);
    assert!(matches!(judge_process(&sample(Degraded, None, true)), Health::Unhealthy(_)));
    assert!(matches!(judge_process(&sample(Unknown, None, true)), Health::Unhealthy(_)));
    // A listener that exits has failed, whatever its code.
    assert!(matches!(judge_process(&sample(Exited, Some(0), true)), Health::Unhealthy(_)));
    match judge_process(&sample(Exited, Some(3), true)) {
        Health::Unhealthy(w) => assert!(w.contains("code 3"), "{w}"),
        h => panic!("{h:?}"),
    }
    // A one-shot run that ends cleanly is finished; a failing one is not.
    assert_eq!(judge_process(&sample(Exited, Some(0), false)), Health::Finished);
    assert!(matches!(judge_process(&sample(Exited, Some(1), false)), Health::Unhealthy(_)));
}

#[test]
fn kinds_define_their_own_polling_and_movability() {
    let kinds = KindRegistry::builtin();
    let cog = kinds.get("cog").unwrap();
    let project = kinds.get("project").unwrap();
    assert!(cog.migratable());
    assert!(!project.migratable(), "a project kernel never moves");
    assert_eq!(cog.health().miss_limit, 3);
    assert!(project.health().interval_ms < cog.health().interval_ms);
    assert_eq!(project.health().miss_limit, 2);
}

#[test]
fn the_default_lifecycle_policy_allows_moving_a_bounded_number_of_times() {
    let p = LifecyclePolicy::default();
    assert!(p.migratable);
    assert!(p.max_reschedules > 0);
}

fn spec_requiring(ids: &[&str]) -> clawft_types::placement::engine::WorkloadSpec {
    use clawft_types::placement::engine::{PlacementPolicy, WorkloadRequirements, WorkloadSpec};
    use clawft_types::placement::{CapabilityId, Requirement};
    WorkloadSpec {
        kind: "cog".into(),
        name: "x".into(),
        requirements: WorkloadRequirements {
            common: ids
                .iter()
                .map(|i| Requirement::exact(CapabilityId::new(*i).unwrap()))
                .collect(),
            ..WorkloadRequirements::default()
        },
        policy: PlacementPolicy::default(),
    }
}

#[test]
fn a_spec_needing_an_attached_sensor_or_device_is_hardware_bound() {
    use super::lifecycle::needs_attached_hardware as bound;
    assert!(!bound(&spec_requiring(&["cpu.arch.aarch64", "os.linux", "runtime.native", "mem.system"])));
    assert!(bound(&spec_requiring(&["os.linux", "sensor.csi.esp32"])));
    assert!(bound(&spec_requiring(&["feed.esp32-csi-udp"])));
    assert!(bound(&spec_requiring(&["device.gpio"])));
    // Not hardware: any node with the same accelerator is as good a home,
    // and trust, store and model requirements describe policy and data.
    for id in ["accel.npu.hailo", "trust.tier.paired", "store.tier.internal", "model.present", "net.egress"] {
        assert!(!bound(&spec_requiring(&[id])), "{id} must not pin");
    }
    // The real cog routes (arch, runtime) are not hardware-bound.
    let routes = super::cog_kind::arch_routes("aarch64").unwrap();
    let mut s = spec_requiring(&[]);
    s.requirements.variants = routes;
    assert!(!bound(&s));
}
