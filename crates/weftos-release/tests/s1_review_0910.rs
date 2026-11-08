//! Failing tests for the HIGH findings of S4's adversarial review of S1's grant seq guard (c43e240a),
//! 2026-11-08 (`/tmp/weftos-owner-0440/s4-review-0910.md`). They are `#[ignore]`d because they fail on S1's
//! branch today: run them with `--ignored` to see the failures; they pass once S1 fixes the finding, and
//! the `#[ignore]` should then be dropped. They test S1's `StoreService`, not S4 code.
#![cfg(feature = "store")]

use std::sync::{Arc, Mutex};

use serde_json::Value;
use weftos_store::audit::MemSink;
use weftos_store::service::{Caller, ChainSink, Config, OpenGate, Parties};
use weftos_store::{MemBackend, Principal, StoreName, StoreService};

const NAME: &str = "weftos://pg-mesh/projects/01JBXZ3N5C8QW4M2R7T9V6YH0K/releases/stable/store";

#[derive(Default)]
struct Chain(Mutex<Vec<(String, Value)>>);
impl ChainSink for Chain {
    fn record(&self, k: &str, v: Value) {
        self.0.lock().unwrap().push((k.to_owned(), v));
    }
}

fn who(id: &str) -> Principal {
    Principal::parse(&format!("cog:{id}")).unwrap()
}

fn svc(state: Option<std::path::PathBuf>) -> (StoreService, Arc<Chain>, StoreName) {
    let mem = Arc::new(MemBackend::new());
    let chain = Arc::new(Chain::default());
    let s = StoreService::open(
        Config { node_id: "pg-1".into(), state_path: state, clock: || 1_000 },
        mem.clone(),
        mem,
        Arc::new(OpenGate),
        Arc::new(MemSink::default()),
        chain.clone(),
    )
    .unwrap();
    (s, chain, StoreName::parse(NAME).unwrap())
}

/// F1. `create_store` hands out the initial grants (defaults plus `Parties`) but its chain event,
/// `store.create`, carries no `seq` and there is no grant event per principal. A consumer that applies
/// grants by `seq` (S3's ledger) cannot learn these grants or order them against a later revoke.
#[test]
#[ignore = "S1 finding F1: store.create grants principals with no seq-stamped event"]
fn initial_grants_reach_the_chain_with_a_seq() {
    let (s, chain, name) = svc(None);
    s.create_store(&name, "op", Parties { writers: vec![who("w1")], ..Default::default() }).unwrap();
    let ev = chain.0.lock().unwrap().clone();
    let stamped = ev.iter().filter(|(k, v)| (k == "store.create" || k.starts_with("store.grant")) && v["seq"].is_u64()).count();
    assert!(stamped >= 1, "no seq-stamped event announces the initial grants: {ev:?}");
}

/// F2. When saving the state fails, `revoke` returns an error but the revoke is already in force in
/// memory, with no chain event and no policy change at the backend. The caller is told "failed" while the
/// service denies the principal; the chain (and so S3's ledger) never sees the revoke. An error must
/// leave the service as it was.
#[test]
#[ignore = "S1 finding F2: a failed state save leaves the change applied in memory, unrecorded"]
fn a_revoke_that_fails_to_persist_is_not_left_half_applied() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("sub").join("state.json");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    let (s, chain, name) = svc(Some(state.clone()));
    let w = Caller::local(who("w1"));
    s.create_store(&name, "op", Parties { writers: vec![who("w1")], ..Default::default() }).unwrap();
    s.put(&w, &name, "k", b"1", false).unwrap();

    // the next save cannot succeed: the state path becomes a non-empty directory
    std::fs::remove_file(&state).unwrap();
    std::fs::create_dir_all(state.join("x")).unwrap();
    let before = chain.0.lock().unwrap().len();
    let r = s.revoke(&name, &who("w1"), "op");
    assert!(r.is_err(), "the save failed, so the call must fail");
    assert_eq!(chain.0.lock().unwrap().len(), before, "an error leaves no chain event");
    assert!(s.put(&w, &name, "k2", b"2", false).is_ok(), "the call failed, so the grant must still stand");
}
