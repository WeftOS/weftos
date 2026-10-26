//! A container cog reaches the broker (ADR-122 gap 1): the host socket is
//! bind-mounted at a fixed path, the cog runs as the uid and gid the broker
//! recorded, and no token ever reaches a command line.

use std::sync::Arc;

use super::container_cmd::Engine;
use super::host_contract::{COGBUS_IN_CONTAINER, CogIdentity, ENV_COGBUS, ENV_COGBUS_TOKEN, HostContract};
use super::test_support::signed_workload;
use super::tests_container::{Recorder, TOML, aarch64_elf, cfg, rt};
use super::types::WorkloadRuntime;

const HOST_SOCK: &str = "/run/weftos/node/cogbus.sock";

fn with_bus(upstream: Option<&str>) -> super::types::WorkloadConfig {
    let mut c = cfg();
    let mut host = HostContract::default_feed();
    if let Some(u) = upstream {
        host = host.with_ingest_upstream(u.parse().unwrap());
    }
    c.host = host
        .with_cogbus_mount(HOST_SOCK)
        .with_run_as(CogIdentity { uid: 60007, gid: 4321, groups: Vec::new() });
    c
}

#[test]
fn the_contract_names_the_in_container_path_and_never_the_host_one() {
    let c = HostContract::default_feed().with_cogbus_mount(HOST_SOCK);
    let env: std::collections::HashMap<_, _> = c.env("/data").into_iter().collect();
    assert_eq!(env[ENV_COGBUS], COGBUS_IN_CONTAINER);
    assert_eq!(env[ENV_COGBUS_TOKEN], c.token.expose());
    assert!(!env.values().any(|v| v == HOST_SOCK), "the host path stays on the host");
    assert_eq!(c.audit()["cogbus_mount"], true);
}

#[tokio::test]
async fn a_plain_image_runs_as_the_recorded_identity_with_the_socket_mounted() {
    let root = tempfile::tempdir().unwrap();
    let rec = Arc::new(Recorder::default());
    let r = rt(Engine::Docker, root.path(), rec.clone());
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let c = with_bus(None);
    let h = r.load(&fx.workload, &c).await.unwrap();
    r.start(&h).await.unwrap();
    let run = rec.find("run").join(" ");
    assert!(run.contains("--user 60007:4321"), "{run}");
    assert!(run.contains(&format!("--mount type=bind,src={HOST_SOCK},dst={COGBUS_IN_CONTAINER}")), "{run}");
    assert!(!run.contains(c.host.token.expose()), "no token on a command line");
    // The image is the same for every instance of the cog: the user is a run flag.
    let build = rec.find("build").join(" ");
    assert!(!build.contains("60007"), "{build}");
    // And the env file the cog reads has the in-container path.
    let env = std::fs::read_to_string(root.path().join(&h.instance_id).join("env")).unwrap();
    assert!(env.contains(&format!("{ENV_COGBUS}={COGBUS_IN_CONTAINER}")), "{env}");
}

#[tokio::test]
async fn a_relay_image_bakes_the_identity_in_and_tags_it() {
    let root = tempfile::tempdir().unwrap();
    let rec = Arc::new(Recorder::default());
    let r = rt(Engine::Docker, root.path(), rec.clone());
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let c = with_bus(Some("192.0.2.10:18080"));
    let h = r.load(&fx.workload, &c).await.unwrap();
    let df = std::fs::read_to_string(root.path().join(&h.instance_id).join("ctx/Dockerfile")).unwrap();
    assert!(df.contains("\"--uid\", \"60007\", \"--gid\", \"4321\""), "{df}");
    r.start(&h).await.unwrap();
    let run = rec.find("run").join(" ");
    assert!(!run.contains("--user"), "the relay drops itself: {run}");
    assert!(run.contains("type=bind"), "{run}");
    assert!(rec.find("build").join(" ").contains("-u60007g4321"), "the identity is in the image tag");
}

#[tokio::test]
async fn an_instance_without_the_bus_runs_exactly_as_before() {
    let root = tempfile::tempdir().unwrap();
    let rec = Arc::new(Recorder::default());
    let r = rt(Engine::Docker, root.path(), rec.clone());
    let fx = signed_workload(TOML, &[("aarch64", &aarch64_elf())]);
    let h = r.load(&fx.workload, &cfg()).await.unwrap();
    r.start(&h).await.unwrap();
    let run = rec.find("run").join(" ");
    assert!(!run.contains("--user") && !run.contains("--mount"), "{run}");
}
