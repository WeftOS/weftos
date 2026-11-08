//! Wire-shape fixtures for the weave socket (card 017ffa84, W2 interim).
//!
//! `tests/fixtures/wire/` is the master copy of the request and response lines Tapestry's
//! hand-written client (`weft-journal`'s `rpc.rs`) exchanges with this daemon. Tapestry holds
//! an identical twin; `sha256sums` is the drift detector. These tests tie every fixture to the
//! daemon's real types: a request fixture must deserialize into `clawft_rpc::Request` and the
//! method's real params type; a response fixture must survive a round trip through the real
//! result type unchanged, and (where the handler can run here) match what the real code emits.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clawft_kernel::chain::ChainManager;
use clawft_kernel::gate::GovernanceGate;
use clawft_kernel::governance::{GovernanceBranch, GovernanceRule, RuleSeverity};
use clawft_rpc::{Request, Response};
use clawft_service_agent::protocol::{ChainAppendParams, ChainAppendResult};
use clawft_weave::capability::{Capability, required_capability};
use clawft_weave::protocol::{
    ChainEventInfo, ChainGetParams, ChainLocalParams, ChainStatusResult, ChainVerifyResult,
    ProcessInfo,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wire")
}

fn load(name: &str) -> Value {
    let p = dir().join(format!("{name}.json"));
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Every fixture the tests below cover. A file in the folder that is not here fails
/// `fixture_set_is_exactly_the_covered_set`, so a new fixture cannot go untested.
const REQUESTS: &[&str] = &[
    "chain.append.request",
    "chain.status.request",
    "chain.tail.request",
    "chain.verify.request",
    "chain.get.request",
    "chain.checkpoint.request",
    "governance.check.request",
    "agent.sources.request",
    "unknown_method.request",
];
const RESPONSES: &[&str] = &[
    "chain.append.response",
    "chain.append.refused.response",
    "chain.status.response",
    "chain.tail.response",
    "chain.verify.response",
    "chain.get.response",
    "chain.get.not_found.response",
    "chain.checkpoint.response",
    "governance.check.response",
    "governance.check.invalid_params.response",
    "agent.sources.response",
    "unknown_method.response",
];

/// Parse `v` into the real type `T` and serialize it back: the fixture must be exactly what
/// the real type writes (no field the type drops, none it would add).
fn roundtrip<T: DeserializeOwned + Serialize>(v: &Value, what: &str) -> T {
    let t: T = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{what}: {e}"));
    assert_eq!(&serde_json::to_value(&t).unwrap(), v, "{what}: not byte-stable through the real type");
    t
}

/// Same keys at every object level and the same JSON type at every leaf. Arrays compare their
/// first element when both have one.
fn same_shape(a: &Value, b: &Value, path: &str) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let kx: Vec<_> = x.keys().collect();
            let ky: Vec<_> = y.keys().collect();
            assert_eq!(kx.len(), ky.len(), "{path}: keys differ: {kx:?} vs {ky:?}");
            for (k, v) in x {
                let w = y.get(k).unwrap_or_else(|| panic!("{path}.{k}: missing in the other side"));
                same_shape(v, w, &format!("{path}.{k}"));
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            if let (Some(p), Some(q)) = (x.first(), y.first()) {
                same_shape(p, q, &format!("{path}[0]"));
            }
        }
        (x, y) => assert_eq!(
            std::mem::discriminant(x),
            std::mem::discriminant(y),
            "{path}: {x} vs {y}"
        ),
    }
}

fn result_of(name: &str) -> Value {
    let v = load(name);
    assert_eq!(v["ok"], true, "{name}");
    v["result"].clone()
}

#[test]
fn fixture_set_is_exactly_the_covered_set() {
    let mut on_disk: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter_map(|n| n.strip_suffix(".json").map(str::to_owned))
        .collect();
    on_disk.sort();
    let mut covered: Vec<String> =
        REQUESTS.iter().chain(RESPONSES).map(|s| (*s).to_owned()).collect();
    covered.sort();
    assert_eq!(on_disk, covered);
}

#[test]
fn sha256sums_match_the_files() {
    let sums = std::fs::read_to_string(dir().join("sha256sums")).expect("sha256sums");
    let listed: BTreeMap<&str, &str> = sums
        .lines()
        .map(|l| {
            let (h, f) = l.split_once("  ").unwrap_or_else(|| panic!("bad line: {l}"));
            (f, h)
        })
        .collect();
    let mut files: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.ends_with(".json"))
        .collect();
    files.sort();
    assert_eq!(listed.keys().copied().collect::<Vec<_>>(), files.iter().map(String::as_str).collect::<Vec<_>>());
    for f in &files {
        let got = format!("{:x}", Sha256::digest(std::fs::read(dir().join(f)).unwrap()));
        assert_eq!(listed[f.as_str()], got, "{f} changed: update sha256sums here and in the Tapestry twin");
    }
}

#[test]
fn every_request_is_a_real_request() {
    for name in REQUESTS {
        let v = load(name);
        // `Request` always writes `"id": null`; the client omits it. Parse, then compare
        // without the null id.
        let req: Request = serde_json::from_value(v.clone()).unwrap();
        let mut again = serde_json::to_value(&req).unwrap();
        if again["id"].is_null() {
            again.as_object_mut().unwrap().remove("id");
        }
        assert_eq!(again, v, "{name}: not stable through the real Request");
        assert_eq!(req.proto, Some(clawft_rpc::PROTO_VERSION), "{name}");
        assert_eq!(req.auth.as_deref(), Some("admin"), "{name}");
        assert!(req.id.is_none() && req.project.is_none() && req.forward.is_none(), "{name}");
        assert_eq!(format!("{}.request", req.method).replace("nonesuch.method.request", "unknown_method.request"), **name);
    }
}

#[test]
fn request_params_fit_the_real_params_types_and_scopes() {
    let p = |n: &str| load(n)["params"].clone();
    let _: ChainAppendParams = roundtrip(&p("chain.append.request"), "chain.append params");
    let get: ChainGetParams = serde_json::from_value(p("chain.get.request")).unwrap();
    assert_eq!((get.sequence, get.signatures), (1, false));
    let tail: ChainLocalParams = serde_json::from_value(p("chain.tail.request")).unwrap();
    assert_eq!(tail.count, 2);
    // The local socket grants "admin"; each method must be inside that and Tapestry's use of
    // them is read-only except append (Write) and checkpoint (Admin).
    for (m, cap) in [
        ("chain.append", Capability::Write),
        ("chain.checkpoint", Capability::Admin),
        ("chain.status", Capability::Read),
        ("chain.tail", Capability::Read),
        ("chain.verify", Capability::Read),
        ("chain.get", Capability::Read),
        ("governance.check", Capability::Read),
        ("agent.sources", Capability::Read),
    ] {
        assert_eq!(required_capability(m), cap, "{m}");
    }
}

fn rule(id: &str, severity: RuleSeverity) -> GovernanceRule {
    GovernanceRule {
        id: id.into(),
        description: format!("test rule {id}"),
        branch: GovernanceBranch::Judicial,
        severity,
        active: true,
        reference_url: None,
        sop_category: None,
        rule_type: Default::default(),
        action_selector: None,
        tool_selector: None,
        force_on_match: false,
    }
}

#[test]
fn governance_check_fixtures_are_what_the_real_handler_emits() {
    let gate = GovernanceGate::new(0.5, false).add_rule(rule("block-shell", RuleSeverity::Blocking));
    let params = load("governance.check.request")["params"].clone();
    let got = clawft_weave::governance_check_rpc::check(Some(&gate), &params);
    assert_eq!(serde_json::to_value(&got).unwrap(), load("governance.check.response"));

    let bad = clawft_weave::governance_check_rpc::check(Some(&gate), &json!({"agent_id": "tapestry"}));
    assert_eq!(serde_json::to_value(&bad).unwrap(), load("governance.check.invalid_params.response"));
}

#[test]
fn error_fixtures_are_real_error_responses() {
    let r: Response = roundtrip(&load("chain.append.refused.response"), "refused");
    let want = Response::error_with_kind("reserved_source", r.error.clone().unwrap());
    assert_eq!(serde_json::to_value(&want).unwrap(), load("chain.append.refused.response"));
    assert!(clawft_kernel::project_identity::is_caller_reserved_source("governance"));

    for name in ["chain.get.not_found.response", "unknown_method.response"] {
        let r: Response = roundtrip(&load(name), name);
        assert!(!r.ok && r.result.is_none() && r.error_kind.is_none(), "{name}");
    }
    // The daemon's dispatch fallthrough is `Response::error(format!("unknown method: {other}"))`
    // (daemon.rs); Tapestry matches on the `unknown method` prefix with no kind.
    let r: Response = roundtrip(&load("unknown_method.response"), "unknown");
    assert_eq!(r.error.as_deref(), Some("unknown method: nonesuch.method"));
}

#[test]
fn success_results_survive_the_real_result_types() {
    let _: ChainAppendResult = roundtrip(&result_of("chain.append.response"), "append result");
    let _: ChainStatusResult = roundtrip(&result_of("chain.status.response"), "status");
    let _: Vec<ChainEventInfo> = roundtrip(&result_of("chain.tail.response"), "tail");
    let _: ChainVerifyResult = roundtrip(&result_of("chain.verify.response"), "verify");
    let _: clawft_kernel::chain_coverage::ChainEventDetail = roundtrip(&result_of("chain.get.response"), "get");
    let doc = result_of("agent.sources.response");
    for n in doc["nodes"].as_array().unwrap() {
        let _: Vec<ProcessInfo> = roundtrip(&n["agents"], "agent rows");
    }
    // Checkpoint: the kernel checkpoint plus the handler's extra keys.
    let mut cp = result_of("chain.checkpoint.response");
    let obj = cp.as_object_mut().unwrap();
    assert_eq!(obj.remove("signed"), Some(json!(false)));
    assert!(obj.remove("reason").is_some());
    let _: clawft_kernel::chain::ChainCheckpoint = roundtrip(&cp, "checkpoint");
}

#[test]
fn success_fixtures_have_the_shape_a_real_chain_produces() {
    let cm = ChainManager::new(0, 100);
    cm.append("kernel", "boot", None);
    let payload = json!({"kind": "tapestry.turn", "entries": ["01JC0000000000000000000001"],
        "hash_before": "aa", "hash_after": "bb", "ts": "2026-11-07T12:00:00+00:00"});
    let ev = cm.append("tapestry", "tapestry.turn", Some(payload));
    let hex = |h: &[u8; 32]| h.iter().map(|b| format!("{b:02x}")).collect::<String>();

    let appended = ChainAppendResult {
        sequence: ev.sequence,
        chain_id: ev.chain_id,
        hash: hex(&ev.hash),
        source: ev.source.clone(),
        kind: ev.kind.clone(),
    };
    same_shape(&serde_json::to_value(appended).unwrap(), &result_of("chain.append.response"), "append");

    let s = cm.status();
    let status = ChainStatusResult {
        chain_id: s.chain_id,
        sequence: s.sequence,
        event_count: s.event_count,
        checkpoint_count: s.checkpoint_count,
        events_since_checkpoint: s.events_since_checkpoint,
        last_hash: hex(&s.last_hash),
    };
    same_shape(&serde_json::to_value(status).unwrap(), &result_of("chain.status.response"), "status");

    let tail: Vec<ChainEventInfo> = cm
        .tail(2)
        .iter()
        .map(|e| ChainEventInfo {
            sequence: e.sequence,
            chain_id: e.chain_id,
            timestamp: e.timestamp.to_rfc3339(),
            source: e.source.clone(),
            kind: e.kind.clone(),
            hash: hex(&e.hash),
            detail: String::new(),
        })
        .collect();
    same_shape(&serde_json::to_value(tail).unwrap(), &result_of("chain.tail.response"), "tail");

    let detail = clawft_kernel::chain_coverage::event_detail(&cm, ev.sequence, false).expect("event 1");
    same_shape(&serde_json::to_value(detail).unwrap(), &result_of("chain.get.response"), "get");
    assert!(clawft_kernel::chain_coverage::event_detail(&cm, 9, false).is_none());

    let v = cm.verify_integrity();
    let verify = ChainVerifyResult {
        valid: v.valid,
        event_count: v.event_count,
        errors: v.errors,
        signature_verified: None,
        signed_through: None,
        pq_verified: None,
    };
    same_shape(&serde_json::to_value(verify).unwrap(), &result_of("chain.verify.response"), "verify");

    let mut cp = serde_json::to_value(cm.checkpoint()).unwrap();
    cp["signed"] = false.into();
    cp["reason"] = "no checkpoint path configured; nothing was signed".into();
    same_shape(&cp, &result_of("chain.checkpoint.response"), "checkpoint");
}
