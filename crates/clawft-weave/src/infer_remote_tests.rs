//! Remote (hosted API) roles and the per-role stall and pin fields: config
//! rules, and one end to end pass through `build` against a fake on a random
//! loopback port. No test reaches the internet; the key variable is `PATH`,
//! which the test process already has.

use serde_json::{Value, json};

use crate::infer_cfg::{FileCfg, load_config, resolve_roles};
use crate::infer_wire::*;
use crate::infer_wire_tests::{free_port, http_get, parts, write_cfg};

fn remote() -> Value {
    json!({"role": "hard", "mode": "remote", "proxy_port": 18_491,
           "remote": {"base_url": "https://api.x.ai/v1", "api_key_env": "GROK_API_KEY", "model": "grok-4"}})
}

fn with(f: impl FnOnce(&mut Value)) -> Value {
    let mut r = remote();
    f(&mut r);
    json!({"roles": [r]})
}

#[test]
fn a_remote_role_is_validated() {
    let dir = tempfile::tempdir().unwrap();
    write_cfg(dir.path(), json!({"roles": [remote()]}));
    assert!(load_config(dir.path()).unwrap().is_some());
    let bad = |v: Value, want: &str| {
        write_cfg(dir.path(), v);
        let e = load_config(dir.path()).unwrap_err();
        assert!(e.contains(want), "{e} should mention {want}");
    };
    bad(with(|r| r["remote"]["base_url"] = "http://api.x.ai/v1".into()), "https");
    bad(with(|r| r["remote"]["base_url"] = "http://localhost:9".into()), "https");
    bad(with(|r| r["remote"]["base_url"] = "https://u:p@api.x.ai/v1".into()), "credentials");
    bad(with(|r| r["remote"]["base_url"] = "nope".into()), "URL");
    bad(with(|r| r["remote"]["api_key_env"] = "sk-live-abc".into()), "NAME");
    bad(with(|r| r["remote"]["api_key_env"] = "".into()), "NAME");
    bad(with(|r| r["remote"]["model"] = "".into()), "model");
    bad(with(|r| { r["remote"].as_object_mut().unwrap().remove("api_key_env"); }), "api_key_env");
    bad(with(|r| r["remote"]["api_key"] = "x".into()), "unknown field");
    bad(with(|r| { r.as_object_mut().unwrap().remove("remote"); }), "needs a remote block");
    bad(with(|r| r["mode"] = "adopted".into()), "needs mode remote");
    // Mixed with the local-server fields: refused, not ignored.
    for (k, v) in [("flavor", json!("llamacpp")), ("instance_port", json!(9)), ("model", json!("m")), ("memory_gb", json!(1)), ("autostart", json!(true)), ("roster_id", json!("x"))] {
        bad(with(|r| r[k] = v.clone()), "a remote role has no");
    }
    bad(with(|r| r["svc"] = json!({"topic": "svc.infer/x", "model": "m"})), "a remote role has no");
    bad(with(|r| { r.as_object_mut().unwrap().remove("proxy_port"); }), "proxy_port");
    bad(with(|r| r["expose"] = json!({"listen": "0.0.0.0", "port": 18_492, "token_file": "t"})), "never exposed");
    let mut v = with(|_| {});
    v["mesh"] = json!({"expose": ["hard"]});
    bad(v, "never shared over the mesh");
}

#[test]
fn stall_timeout_and_pin_model_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let local = |f: &dyn Fn(&mut Value)| {
        let mut r = json!({"role": "mlx", "flavor": "mlx-lm", "instance_port": 18_083, "model": "loaded"});
        f(&mut r);
        json!({"roles": [r]})
    };
    for ok in [10, 124, 300, 900] {
        write_cfg(dir.path(), local(&|r| r["stall_timeout_s"] = ok.into()));
        assert!(load_config(dir.path()).is_ok(), "{ok}");
    }
    for bad in [0, 9, 901, 100_000] {
        write_cfg(dir.path(), local(&|r| r["stall_timeout_s"] = bad.into()));
        assert!(load_config(dir.path()).unwrap_err().contains("stall_timeout_s"), "{bad}");
    }
    write_cfg(dir.path(), local(&|r| r["stall_timeout_s"] = (-1).into()));
    assert!(load_config(dir.path()).is_err());
    write_cfg(dir.path(), local(&|r| r["pin_model"] = true.into()));
    assert!(load_config(dir.path()).is_ok());
    write_cfg(dir.path(), json!({"roles": [{"role": "v", "svc": {"topic": "svc.infer/x", "model": "m"}, "pin_model": true}]}));
    assert!(load_config(dir.path()).unwrap_err().contains("svc role has no stall_timeout_s"));
    // pin_model needs a model to pin to.
    let cfg: FileCfg = serde_json::from_value(json!({"roles": [{"role": "a", "flavor": "llamacpp", "instance_port": 9, "pin_model": true}]})).unwrap();
    assert!(resolve_roles(&cfg, None).unwrap_err().contains("pin_model needs a model"));
}

#[test]
fn a_remote_role_resolves_without_a_server() {
    let cfg: FileCfg = serde_json::from_value(json!({"roles": [remote()]})).unwrap();
    let r = resolve_roles(&cfg, None).unwrap();
    assert!(!r[0].managed);
    assert_eq!(r[0].spec.model.as_deref(), Some("grok-4"));
}

#[tokio::test]
async fn a_remote_role_forwards_through_the_daemon_wiring() {
    let up = crate::infer_wire_tests::fake().await;
    let port = free_port();
    let dir = tempfile::tempdir().unwrap();
    write_cfg(
        dir.path(),
        json!({"roles": [{"role": "hard", "mode": "remote", "proxy_port": port, "stall_timeout_s": 30,
            "remote": {"base_url": format!("http://{}/v1", up.addr), "api_key_env": "PATH", "model": "grok-4"}}]}),
    );
    let (state, _cfg) = build(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
    let st = state.role_json_for_test("hard");
    assert_eq!((st["mode"].as_str(), st["state"].as_str()), (Some("remote"), Some("serving")), "{st}");
    assert!(!st.to_string().contains(&std::env::var("PATH").unwrap()), "status never carries the key");
    let resp = http_get(([127, 0, 0, 1], port).into(), "/v1/models").await;
    assert!(resp.contains("m.gguf"), "{resp}");
    assert_eq!(up.seen.lock().unwrap().last().unwrap(), "GET /v1/models HTTP/1.1");
}

#[tokio::test]
async fn status_reports_a_missing_key_variable_by_name() {
    let dir = tempfile::tempdir().unwrap();
    write_cfg(
        dir.path(),
        json!({"roles": [{"role": "hard", "mode": "remote", "proxy_port": free_port(),
            "remote": {"base_url": "https://api.x.ai/v1", "api_key_env": "WEFTOS_TEST_KEY_NOT_SET", "model": "grok-4"}}]}),
    );
    let (state, _cfg) = build(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
    let st = state.role_json_for_test("hard");
    assert_eq!(st["state"], "waiting");
    assert!(st["reason"].as_str().unwrap().contains("WEFTOS_TEST_KEY_NOT_SET"), "{st}");
}
