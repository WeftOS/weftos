//! Broker rate-limit state stays bounded, and the lease future is `Send` for a
//! `Send` minter (BR RT-04-5).

use std::sync::Arc;

use crate::broker::{BrokerConfig, EdgeBroker};
use crate::clock::ManualClock;
use crate::mock::{MockRustfs, StaticGrants, VecAudit};
use crate::policy::Access;
use crate::wire::{Endpoint, LeaseRequest, RefusalCode};

const T0: u64 = 1_800_000_000;

type Broker = EdgeBroker<MockRustfs, StaticGrants, VecAudit, ManualClock>;

fn rig() -> (Arc<Broker>, ManualClock, StaticGrants) {
    let clock = ManualClock::new(T0);
    let store = MockRustfs::new(clock.clone());
    let grants = StaticGrants::default();
    let cfg = BrokerConfig::new(Endpoint { scheme: "http".into(), host: "pg.tailnet:9000".into() });
    let broker = Arc::new(EdgeBroker::new(cfg, store, grants.clone(), VecAudit::default(), clock.clone()));
    (broker, clock, grants)
}

fn run<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
}

#[test]
fn rate_state_forgets_nodes_that_stop_calling() {
    let (broker, clock, grants) = rig();
    for i in 0..50 {
        let node = format!("node-{i}");
        grants.grant(&node, &[Access::Write]);
        run(broker.lease(&node, LeaseRequest::Sts)).unwrap();
    }
    assert_eq!(broker.tracked_nodes(), 50);
    // Every window has passed; one node comes back.
    clock.advance(BrokerConfig::new(Endpoint { scheme: "http".into(), host: "h".into() }).rate_window_secs + 1);
    run(broker.lease("node-0", LeaseRequest::Sts)).unwrap();
    assert_eq!(broker.tracked_nodes(), 1, "idle nodes must not stay in the rate table forever");
}

#[test]
fn rate_limit_still_applies_within_the_window_and_resets_after_it() {
    let (broker, clock, grants) = rig();
    grants.grant("n", &[Access::Write]);
    let cfg = BrokerConfig::new(Endpoint { scheme: "http".into(), host: "h".into() });
    for _ in 0..cfg.rate_max {
        run(broker.lease("n", LeaseRequest::Sts)).unwrap();
    }
    let e = run(broker.lease("n", LeaseRequest::Sts)).unwrap_err();
    assert_eq!(e.code, RefusalCode::RateLimited);
    clock.advance(cfg.rate_window_secs);
    run(broker.lease("n", LeaseRequest::Sts)).unwrap();
}

fn assert_send<T: Send>(_: &T) {}

#[test]
fn lease_future_is_send_for_a_send_minter() {
    let (broker, _, grants) = rig();
    grants.grant("n", &[Access::Write]);
    let fut = broker.lease("n", LeaseRequest::Sts);
    assert_send(&fut);
    let fut2 = broker.revoke("n");
    assert_send(&fut2);
    drop((fut, fut2));
}
