//! The daemon's registrar: what a placed cog's manifest becomes in the
//! broker, and what the cog is handed.

use std::net::SocketAddr;

use clawft_kernel::workload_runtime::host_contract::{ENV_COGBUS, ENV_COGBUS_TOKEN};
use clawft_kernel::workload_runtime::{CogSpec, HostContract};
use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::json;

use super::*;

const PROJECT: &str = "01J9ZXW0PRJCTAAAAAAAAAAAAA";

const COG_TOML: &str = r#"
[cog]
id = "board-telemetry"
version = "0.1.0"

[[outputs]]
topic = "telemetry"
schema = "telemetry.v1"
class = "reading"
rate = "1 Hz"

[[inputs]]
topic = "detections"
schema = "detections.v1"
policy = "sample"
"#;

fn registration(contract: &HostContract, uid: Option<u32>) -> CogbusRegistration {
    let spec = CogSpec::parse(COG_TOML.as_bytes()).unwrap();
    CogbusRegistration {
        instance_id: "board-telemetry-1".into(),
        cog_id: spec.id,
        project_id: Some(PROJECT.into()),
        controller_node: "ctl".into(),
        token: contract.token.clone(),
        uid,
        isolation: clawft_kernel::workload_ctl::host_cogbus::Isolation::SharedUid,
        outputs: spec.outputs,
        inputs: spec.inputs,
    }
}

#[test]
fn the_kernel_and_the_broker_agree_on_the_env_names() {
    assert_eq!(ENV_COGBUS, weftos_cogbus::ENV_SOCKET);
    assert_eq!(ENV_COGBUS_TOKEN, weftos_cogbus::ENV_TOKEN);
}

#[tokio::test]
async fn a_placed_manifest_becomes_the_broker_entry_and_the_cog_env() {
    let tmp = tempfile::tempdir().unwrap();
    let rt = start(tmp.path(), &"0".repeat(32)).await.unwrap();
    assert_eq!(rt.socket(), tmp.path().join("cogbus.sock"));
    let reg = Registrar::new(rt.broker.clone(), rt.socket());

    // Native cogs get the socket and the daemon's uid; other routes nothing.
    let a = reg.attach("native").unwrap();
    assert_eq!(a.socket, rt.socket().to_string_lossy());
    assert_eq!(a.uid, Some(nix::unistd::geteuid().as_raw()));
    assert!(reg.attach("container").is_none());

    let contract = HostContract::new(SocketAddr::from(([0, 0, 0, 0], 5006))).with_cogbus(a.socket.clone());
    let env: std::collections::HashMap<_, _> = contract.env("/data").into_iter().collect();
    assert_eq!(env[ENV_COGBUS], a.socket);
    assert_eq!(env[ENV_COGBUS_TOKEN], contract.token.expose(), "the instance token, reused");
    assert!(contract.audit().to_string().contains("cogbus"));
    assert!(!contract.audit().to_string().contains(contract.token.expose()), "no token in the audit");

    let r = registration(&contract, a.uid);
    reg.register(&r).unwrap();
    let entry = rt.broker.instance("board-telemetry-1").unwrap();
    assert_eq!(entry.cog_id, "board-telemetry");
    assert_eq!(entry.project_id.as_deref(), Some(PROJECT));
    assert_eq!(entry.uid, a.uid);
    assert_eq!(entry.outputs, vec![OutputSpec { topic: "telemetry".into(), schema: "telemetry.v1".into(), class: TopicClass::Reading }]);
    assert_eq!(entry.inputs, vec![InputSpec { topic: "detections".into(), schema: "detections.v1".into(), policy: InputPolicy::Sample }]);

    // The cog connects with exactly what it was handed and publishes.
    let c = weftos_cogbus::Client::connect(Path::new(&env[ENV_COGBUS]), &env[ENV_COGBUS_TOKEN]).await.unwrap();
    assert_eq!(c.publish("telemetry", "venq-board", "telemetry.v1", 1, json!({"items": []})).await.unwrap(), 1);
    let doc = list_json(Some(&rt));
    assert_eq!(doc["running"], true);
    assert_eq!(doc["topics"][0]["topic"], "telemetry/venq-board");
    assert_eq!(doc["topics"][0]["writer"], "board-telemetry-1");

    // Unload revokes; the token no longer opens a session.
    reg.revoke("board-telemetry-1");
    assert!(rt.broker.instance("board-telemetry-1").is_none());
    assert!(weftos_cogbus::Client::connect(rt.socket(), contract.token.expose()).await.is_err());
    assert_eq!(list_json(None)["running"], false);
}

#[tokio::test]
async fn a_socket_path_that_cannot_be_bound_is_an_error_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("cogbus.sock"), "not a socket").unwrap();
    assert!(start(tmp.path(), "n").await.is_err());
}

#[tokio::test]
async fn the_daemons_broker_has_a_content_store_under_the_runtime_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let broker = broker_in("n", BusHooks::default(), tmp.path());
    assert!(broker.blob_store().is_some());
    assert!(tmp.path().join(BLOB_DIR).is_dir());
    let rt = start_on(broker, tmp.path()).await.unwrap();
    let doc = list_json(Some(&rt));
    assert_eq!(doc["blobs"]["quota"], 2u64 << 30);
    assert_eq!(doc["blobs"]["blobs"], 0);
}

#[tokio::test]
async fn a_declared_rate_becomes_a_budget_and_a_flood_asks_the_host_to_stop_the_cog() {
    use std::sync::Mutex;
    use clawft_kernel::workload_ctl::host_cogbus::CogbusRegistrar;
    let tmp = tempfile::tempdir().unwrap();
    let (hooks, activity) = hooks_with_activity(tmp.path(), None);
    let asked: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let sink = asked.clone();
    assert!(activity.set_stop_hook(Arc::new(move |i, r| sink.lock().unwrap().push((i.into(), r.into())))));
    let broker = broker_in("n", hooks, tmp.path());
    let reg = Registrar::new(broker.clone(), &tmp.path().join("cogbus.sock"));
    let mut r = registration(&HostContract::new(SocketAddr::from(([0, 0, 0, 0], 5006))), None);
    r.token = clawft_types::secret::SecretString::new("t".repeat(40));
    assert_eq!(r.outputs[0].rate.as_deref(), Some("1 Hz"));
    reg.register(&r).unwrap();

    let mut s = broker.authenticate(&"t".repeat(40), None).unwrap();
    let publish = |i: u64| broker.publish(&s, "telemetry", "venq-board", "telemetry.v1", i, json!({"items": []}));
    // 1 Hz declared: a burst of 8, then refused.
    let mut limited = 0;
    for i in 0..60 {
        if let Err(e) = publish(i) {
            assert_eq!(e.code, weftos_cogbus::ErrorCode::RateLimited, "{e:?}");
            limited += 1;
            if broker.record_refusal(&e, "board-telemetry-1") {
                break;
            }
        }
    }
    assert!(limited >= 20, "{limited}");
    let asked = asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(asked[0].0, "board-telemetry-1");
    assert!(asked[0].1.contains("rate_limited"), "{}", asked[0].1);
    let _ = &mut s;
}
