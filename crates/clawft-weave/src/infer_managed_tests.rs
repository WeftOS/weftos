//! Managed roles, the roster, the memory budget, exposure beyond loopback
//! and the consumers, in the daemon. The launcher is a fake script, the
//! "model server" a loopback stub brought up after it, the roster a
//! fixture copy, and the model weights a few bytes in a temp dir. No real
//! server, `~/llm`, or model file is touched.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clawft_kernel::chain::ChainManager;
use clawft_kernel::model_manifest::{AdoptInput, ModelFormat, ModelRegistry, ModelSource, ModelTrust, scan_file};
use clawft_kernel::revocation::RevocationList;
use clawft_kernel::workload_governance::{NetworkPolicy, PackageTrust, WorkloadGate, WorkloadPermitRule};
use clawft_kernel::workload_pkg::codec::hex_encode;
use clawft_kernel::workload_pkg::{KeyOrigin, TrustAnchors, key_id_for};
use clawft_kernel::workload_runtime::host::RUNTIME_CHAIN_SOURCE;

use crate::infer_wire::*;
use crate::infer_wire_tests::{Audit, fake_on, free_port, http_get, parts, write_cfg};

const ROSTER: &str = include_str!("../../clawft-kernel/src/workload_runtime/infer/fixtures/model-lab-roster.yaml");

pub(crate) struct Lab {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) chain: Arc<ChainManager>,
}

pub(crate) fn permit(max_network: NetworkPolicy) -> WorkloadPermitRule {
    let mut r = WorkloadPermitRule::new("permit-infer", ["workload.*"], ["inference"]);
    r.min_package_trust = PackageTrust::OperatorAttested;
    r.max_network = max_network;
    r
}

fn gate(chain: &Arc<ChainManager>, dir: &Path, rule: Option<WorkloadPermitRule>) -> Arc<WorkloadGate> {
    let rev = Arc::new(RevocationList::new(dir.join("revocations.json")));
    let g = WorkloadGate::new(0.95, false, rev).with_chain(chain.clone());
    Arc::new(match rule {
        Some(r) => g.with_permit(r).unwrap(),
        None => g,
    })
}

/// A runtime dir with a fake launcher, an operator-trusted model registry
/// holding `Model-A`/`Model-B` (gguf), and a chain.
pub(crate) fn lab() -> Lab {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let bin = p.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["serve-llamacpp", "serve"] {
        let f = bin.join(name);
        std::fs::write(&f, "#!/bin/sh\nd=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$d/argv-$$.txt\"\necho $$ > \"$d/pid.txt\"\nexec sleep 600\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
    let pk = key.verifying_key().to_bytes();
    let id = key_id_for(&pk);
    std::fs::write(
        p.join("workload-trust.json"),
        serde_json::json!({"schema": "weftos.workload-trust.v1",
            "operator_keys": [{"key_id": id, "public_key": hex_encode(&pk)}]})
        .to_string(),
    )
    .unwrap();
    let mut anchors = TrustAnchors::default();
    anchors.push_signer(&id, &hex_encode(&pk), KeyOrigin::Operator).unwrap();
    std::fs::create_dir_all(p.join("models")).unwrap();
    let reg = ModelRegistry::open(p.join("models").join("registry.json")).unwrap();
    reg.set_trust(ModelTrust::new(anchors.clone()));
    for n in ["Model-A", "Model-B"] {
        let f = p.join(format!("{n}.gguf"));
        std::fs::write(&f, vec![3u8; 2048]).unwrap();
        let scanned = scan_file(
            &f,
            AdoptInput {
                name: n.into(),
                format: ModelFormat::Gguf,
                source: ModelSource { hf_repo: Some("example-org/example".into()), ..ModelSource::default() },
                redistributable: false,
            },
        )
        .unwrap();
        reg.adopt(scanned, &key, &id, &anchors, false).unwrap();
    }
    Lab { dir, chain: Arc::new(ChainManager::new(0, 1000)) }
}

impl Lab {
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }
    pub(crate) fn serve(&self) -> serde_json::Value {
        serde_json::json!({"llamacpp": self.path().join("bin/serve-llamacpp"), "mlx-lm": self.path().join("bin/serve")})
    }
    pub(crate) fn parts(&self, g: Option<Arc<WorkloadGate>>) -> InitParts<'_> {
        let mut p = parts(self.path(), None, false, None);
        p.gate = g;
        p.chain = Some(self.chain.clone());
        p
    }
    pub(crate) fn gate(&self, rule: Option<WorkloadPermitRule>) -> Arc<WorkloadGate> {
        gate(&self.chain, self.path(), rule)
    }
    fn refusals(&self) -> Vec<String> {
        self.chain
            .tail(0)
            .into_iter()
            .filter(|e| e.source == RUNTIME_CHAIN_SOURCE && e.kind == "workload.refuse")
            .map(|e| e.payload.map(|p| p.to_string()).unwrap_or_default())
            .collect()
    }
}

pub(crate) fn managed(role: &str, model: &str, gb: f64, instance: u16, proxy: Option<u16>) -> serde_json::Value {
    let mut r = serde_json::json!({"role": role, "mode": "managed", "flavor": "llamacpp", "model": model,
        "memory_gb": gb, "instance_port": instance});
    if let Some(p) = proxy {
        r["proxy_port"] = p.into();
    }
    r
}

pub(crate) async fn wait_for(p: &Path) -> bool {
    for _ in 0..800 {
        if p.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    false
}

pub(crate) fn state_of(st: &InferState, role: &str) -> (String, Option<String>) {
    let v = st.role_json_for_test(role);
    (v["state"].as_str().unwrap().to_string(), v["reason"].as_str().map(str::to_string))
}

#[tokio::test]
async fn a_managed_role_starts_through_the_launcher_serves_and_stops() {
    let lab = lab();
    let (instance, proxy_port) = (free_port(), free_port());
    write_cfg(
        lab.path(),
        serde_json::json!({"roles": [managed("hermes", "Model-A", 6.0, instance, Some(proxy_port))],
            "serve_programs": lab.serve(), "budget_gb": 96}),
    );
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::None)))))).await.unwrap().unwrap();
    // Off until asked: nothing launched, nothing served.
    assert_eq!(state_of(&st, "hermes").0, "stopped");
    assert!(!lab.path().join("bin/pid.txt").exists());
    assert!(st.table.resolve("hermes").is_none());

    st.start_role("hermes").await.unwrap();
    assert!(wait_for(&lab.path().join("bin/pid.txt")).await, "the launcher ran");
    assert_eq!(state_of(&st, "hermes").0, "starting");
    // The server comes up on its own port; the next pass serves it.
    let server = fake_on(instance).await;
    st.sync_once().await;
    assert_eq!(state_of(&st, "hermes").0, "running");
    assert!(st.table.resolve("hermes").is_some());
    let r = http_get(([127, 0, 0, 1], proxy_port).into(), "/v1/chat/completions").await;
    assert!(r.contains("served"), "{r}");
    assert!(server.seen.lock().unwrap().iter().any(|l| l.contains("/v1/chat/completions")));
    assert_eq!(st.ledger.used(), 6_000_000_000);
    // The launch carried the registry's weights path and a loopback host.
    let argv = std::fs::read_dir(lab.path().join("bin"))
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().starts_with("argv-"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .unwrap();
    assert!(argv.contains("Model-A.gguf") && argv.contains("--host\n127.0.0.1"), "{argv}");
    // Every governed step is chained.
    let kinds: Vec<String> = lab.chain.tail(0).into_iter().filter(|e| e.source == RUNTIME_CHAIN_SOURCE).map(|e| e.kind).collect();
    assert!(kinds.contains(&"workload.load".to_string()) && kinds.contains(&"workload.start".to_string()), "{kinds:?}");

    st.stop_role("hermes").await.unwrap();
    assert_eq!(state_of(&st, "hermes").0, "stopped");
    assert!(st.table.resolve("hermes").is_none());
    assert_eq!(st.ledger.used(), 0, "the memory was given back");
    let kinds: Vec<String> = lab.chain.tail(0).into_iter().filter(|e| e.source == RUNTIME_CHAIN_SOURCE).map(|e| e.kind).collect();
    assert!(kinds.contains(&"workload.stop".to_string()) && kinds.contains(&"workload.unload".to_string()), "{kinds:?}");
}

#[tokio::test]
async fn autostart_is_off_unless_asked_and_a_denied_gate_stops_the_launch() {
    let lab = lab();
    let mut on = managed("a", "Model-A", 1.0, free_port(), None);
    on["autostart"] = true.into();
    write_cfg(lab.path(), serde_json::json!({"roles": [on], "serve_programs": lab.serve()}));
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::None)))))).await.unwrap().unwrap();
    assert_eq!(state_of(&st, "a").0, "starting", "autostart launched it on the first pass");
    st.shutdown().await;
    assert_eq!(st.ledger.used(), 0, "shutdown stops what this daemon started");

    // Default deny: no permit rule, so the load is refused and chained.
    let lab = lab_with_cfg_and_gate(None);
    let (st, _) = build(lab.0.parts(Some(lab.1))).await.unwrap().unwrap();
    let r = st.start_role("hermes").await.unwrap();
    assert_eq!(r["state"], "refused", "{r}");
    assert!(r["reason"].as_str().unwrap().contains("denied"), "{r}");
    assert!(!lab.0.path().join("bin/pid.txt").exists(), "nothing was launched without a permit");
}

fn lab_with_cfg_and_gate(rule: Option<WorkloadPermitRule>) -> (Lab, Arc<WorkloadGate>) {
    let lab = lab();
    write_cfg(
        lab.path(),
        serde_json::json!({"roles": [managed("hermes", "Model-A", 1.0, free_port(), None)], "serve_programs": lab.serve()}),
    );
    let g = lab.gate(rule);
    (lab, g)
}

#[tokio::test]
async fn managed_roles_are_unavailable_not_guessed_without_their_prerequisites() {
    let lab = lab();
    // No launcher configured for the flavor: never falls back to a guessed path.
    write_cfg(lab.path(), serde_json::json!({"roles": [managed("hermes", "Model-A", 1.0, free_port(), None)]}));
    let (st, _) = build(lab.parts(Some(lab.gate(None)))).await.unwrap().unwrap();
    let (state, why) = state_of(&st, "hermes");
    assert_eq!(state, "unavailable");
    assert!(why.unwrap().contains("serve_programs"));
    assert!(st.start_role("hermes").await.is_err());
    // A launcher that is not executable.
    std::fs::set_permissions(lab.path().join("bin/serve-llamacpp"), std::fs::Permissions::from_mode(0o644)).unwrap();
    write_cfg(
        lab.path(),
        serde_json::json!({"roles": [managed("hermes", "Model-A", 1.0, free_port(), None)], "serve_programs": lab.serve()}),
    );
    let (st, _) = build(lab.parts(Some(lab.gate(None)))).await.unwrap().unwrap();
    assert!(state_of(&st, "hermes").1.unwrap().contains("not an executable"));
    // No gate: managed roles are governed, so they stay off.
    std::fs::set_permissions(lab.path().join("bin/serve-llamacpp"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let (st, _) = build(lab.parts(None)).await.unwrap().unwrap();
    assert!(state_of(&st, "hermes").1.unwrap().contains("governance gate"));
}

#[tokio::test]
async fn an_over_budget_or_excluded_start_is_unplaceable_chained_and_clears_when_memory_frees() {
    let lab = lab();
    let mut planner = managed("planner", "Model-B", 6.0, free_port(), None);
    planner["excludes"] = serde_json::json!(["role:swarm"]);
    write_cfg(
        lab.path(),
        serde_json::json!({"roles": [
            managed("coder", "Model-A", 6.0, free_port(), None),
            planner,
            managed("swarm", "Model-A", 1.0, free_port(), None)],
            "serve_programs": lab.serve(), "budget_gb": 10}),
    );
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::None)))))).await.unwrap().unwrap();
    st.start_role("coder").await.unwrap();
    // 6 + 6 > 10: unplaceable, with the reason on the role and on the chain.
    let r = st.start_role("planner").await.unwrap();
    assert_eq!(r["state"], "unplaceable", "{r}");
    let why = r["reason"].as_str().unwrap();
    assert!(why.contains("'planner' needs 6.0 GB") && why.contains("coder 6.0 GB") && why.contains("10.0 GB"), "{why}");
    let chained = lab.refusals().join(" ");
    assert!(chained.contains("unplaceable") && chained.contains("planner"), "{chained}");
    assert_eq!(st.ledger.used(), 6_000_000_000, "the refusal reserved nothing");
    // The status shows the budget and who holds it.
    let status = crate::infer_rpc::status_for_test(&st);
    assert_eq!(status["memory"]["budget_gb"], 10.0);
    assert_eq!(status["memory"]["resident"][0]["role"], "coder");
    // Retried after a pause, so the tick does not hammer the chain.
    let n = lab.refusals().len();
    st.sync_once().await;
    assert_eq!(lab.refusals().len(), n, "no retry inside the pause");
    // Freeing the memory lets an explicit start through.
    st.stop_role("coder").await.unwrap();
    assert_eq!(state_of(&st, "planner").0, "unplaceable");
    let r = st.start_role("planner").await.unwrap();
    assert_ne!(r["state"], "unplaceable", "{r}");
    // Co-residency: swarm is excluded by the running planner, though 1 GB fits.
    let r = st.start_role("swarm").await.unwrap();
    assert_eq!(r["state"], "unplaceable", "{r}");
    assert!(r["reason"].as_str().unwrap().contains("'planner' excludes it") || r["reason"].as_str().unwrap().contains("cannot be resident beside"), "{r}");
    st.shutdown().await;
}

#[tokio::test]
async fn roles_take_their_facts_from_the_roster_and_explicit_fields_win() {
    let lab = lab();
    let roster = lab.path().join("queue.yaml");
    std::fs::write(&roster, ROSTER).unwrap();
    let (coder_port, planner_port) = (free_port(), free_port());
    write_cfg(
        lab.path(),
        serde_json::json!({
            "roster": {"file": roster, "overlay": {"excludes": {"planner": ["role:coder-daily"]}}},
            "roles": [
                {"role": "coder-daily", "mode": "managed", "roster_id": "coder-daily", "flavor": "llamacpp", "model": "Model-A", "instance_port": coder_port},
                {"role": "planner", "mode": "managed", "roster_id": "planner", "flavor": "llamacpp", "model": "Model-B", "instance_port": planner_port, "memory_gb": 40}],
            "serve_programs": lab.serve(), "budget_gb": 96}),
    );
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::None)))))).await.unwrap().unwrap();
    let daily = st.role_json_for_test("coder-daily");
    assert_eq!(daily["flavor"], "infer.llamacpp", "an explicit flavor beats the roster's mlx_lm");
    assert_eq!(daily["memory_gb"], 55.0, "ram_gb from the roster");
    assert_eq!(daily["model"], "Model-A", "an explicit model beats the roster's name");
    assert_eq!(daily["instance_port"], coder_port, "an explicit port beats the roster's 8081");
    let planner = st.role_json_for_test("planner");
    assert_eq!(planner["memory_gb"], 40.0, "an explicit memory beats the roster's 55");
    // The roster's overlay made them exclusive.
    st.start_role("coder-daily").await.unwrap();
    let r = st.start_role("planner").await.unwrap();
    assert_eq!(r["state"], "unplaceable", "{r}");
    assert!(r["reason"].as_str().unwrap().contains("'planner' excludes it"), "{r}");
    st.shutdown().await;
    // What the roster could not import is reported.
    assert!(crate::infer_rpc::status_for_test(&st)["roster_skipped"].as_array().unwrap().iter().any(|e| e["id"] == "vlm"));

    // A roster entry that cannot be imported is a boot error with its reason.
    write_cfg(
        lab.path(),
        serde_json::json!({"roster": {"file": roster}, "roles": [{"role": "x", "roster_id": "vlm", "instance_port": 9}]}),
    );
    let e = build(lab.parts(None)).await.err().unwrap();
    assert!(e.contains("vlm") && e.contains("no inference adapter"), "{e}");
    // The real roster is only ever named by the operator: a missing file is an error, not a guess.
    write_cfg(
        lab.path(),
        serde_json::json!({"roster": {"file": "/nonexistent/queue.yaml"}, "roles": [{"role": "x", "roster_id": "coder-daily", "instance_port": 9}]}),
    );
    assert!(build(lab.parts(None)).await.is_err());
}

fn token_file(lab: &Lab, mode: u32, text: &str) -> &'static str {
    let rel = "secrets/infer/hermes.token";
    let p: PathBuf = lab.path().join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    for d in [p.parent().unwrap().parent().unwrap(), p.parent().unwrap()] {
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let _ = std::fs::remove_file(&p);
    std::fs::write(&p, text).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
    rel
}

const TOKEN: &str = "t0ken-0123456789-abcdefghijklmnopqrstuvwxyz";

fn exposed_cfg(lab: &Lab, instance: u16, proxy: u16) -> serde_json::Value {
    let mut r = serde_json::json!({"role": "hermes", "flavor": "llamacpp", "instance_port": instance, "proxy_port": proxy,
        "expose": {"listen": "127.0.0.1", "port": free_port(), "token_file": "secrets/infer/hermes.token"}});
    let _ = lab;
    r["provider"] = serde_json::Value::Null;
    r.as_object_mut().unwrap().remove("provider");
    serde_json::json!({"roles": [r]})
}

#[tokio::test]
async fn beyond_loopback_needs_a_token_and_a_chained_permit_with_network_lan() {
    let up = fake_on(0).await;
    let lab = lab();
    let proxy = free_port();
    write_cfg(lab.path(), exposed_cfg(&lab, up.addr.port(), proxy));
    let state = |st: &InferState| st.role_json_for_test("hermes")["exposed_proxy"].clone();
    let refused = |v: serde_json::Value| v["refused"].as_str().unwrap_or("").to_string();

    // No gate: refused.
    token_file(&lab, 0o600, TOKEN);
    let (st, _) = build(lab.parts(None)).await.unwrap().unwrap();
    assert!(refused(state(&st)).contains("governance gate"), "{:?}", state(&st));
    // Token problems: readable by others, too short, a symlink.
    token_file(&lab, 0o644, TOKEN);
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::Lan)))))).await.unwrap().unwrap();
    assert!(refused(state(&st)).contains("chmod 600"), "{:?}", state(&st));
    token_file(&lab, 0o600, "short");
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::Lan)))))).await.unwrap().unwrap();
    assert!(refused(state(&st)).contains("exposure token"), "{:?}", state(&st));
    let real = lab.path().join("real.token");
    std::fs::write(&real, TOKEN).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = lab.path().join("secrets/infer/hermes.token");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::Lan)))))).await.unwrap().unwrap();
    assert!(refused(state(&st)).contains("a link is refused"), "{:?}", state(&st));
    std::fs::remove_file(&link).unwrap();
    // A parent directory others can write to (or a link in the path): refused.
    token_file(&lab, 0o600, TOKEN);
    std::fs::set_permissions(lab.path().join("secrets/infer"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::Lan)))))).await.unwrap().unwrap();
    assert!(refused(state(&st)).contains("must not be writable"), "{:?}", state(&st));
    token_file(&lab, 0o600, TOKEN);
    let moved = lab.path().join("secrets-real");
    std::fs::rename(lab.path().join("secrets"), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, lab.path().join("secrets")).unwrap();
    let (st, _) = build(lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::Lan)))))).await.unwrap().unwrap();
    assert!(refused(state(&st)).contains("not a real directory"), "{:?}", state(&st));
    std::fs::remove_file(lab.path().join("secrets")).unwrap();
    std::fs::rename(&moved, lab.path().join("secrets")).unwrap();
    token_file(&lab, 0o600, TOKEN);

    // Default deny, and a permit that does not allow lan: the gate refuses.
    for rule in [None, Some(permit(NetworkPolicy::None))] {
        let (st, _) = build(lab.parts(Some(lab.gate(rule)))).await.unwrap().unwrap();
        assert!(refused(state(&st)).contains("governance denied"), "{:?}", state(&st));
    }
    assert!(lab.chain.tail(0).iter().any(|e| e.kind == "workload.refuse" || e.kind == "workload.start"), "the gate chained its decisions");

    // A lan permit: the proxy listens, behind the token. (Fresh ports: the
    // states above still hold their listeners.)
    write_cfg(lab.path(), exposed_cfg(&lab, up.addr.port(), free_port()));
    let audit = Arc::new(Audit::default());
    let mut p = lab.parts(Some(lab.gate(Some(permit(NetworkPolicy::Lan)))));
    p.audit = Some(audit.clone());
    let (st, _) = build(p).await.unwrap().unwrap();
    let ProxyState::Exposed(addr) = st.roles[0].exposed_proxy.lock().unwrap().clone().unwrap() else {
        panic!("{:?}", state(&st))
    };
    // The role's own proxy_port stays a token-free loopback listener for
    // local consumers; the exposed one demands the token.
    let ProxyState::Listening(local) = st.roles[0].proxy.lock().unwrap().clone() else { panic!() };
    assert_ne!(local, addr);
    assert!(http_get(local, "/v1/models").await.starts_with("HTTP/1.1 200"), "local consumers need no token");
    let r = http_get(addr, "/v1/models").await;
    assert!(r.starts_with("HTTP/1.1 401"), "{r}");
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut s, format!("GET /v1/models HTTP/1.1\r\nHost: anything\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut s, &mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out).starts_with("HTTP/1.1 200"), "{}", String::from_utf8_lossy(&out));
    assert!(!format!("{:?}", audit.0.lock().unwrap()).contains(TOKEN), "the token reached the audit trail");
}

#[tokio::test]
async fn the_default_is_loopback_only() {
    // A role with no `expose` never listens beyond 127.0.0.1.
    let up = fake_on(0).await;
    let dir = tempfile::tempdir().unwrap();
    write_cfg(dir.path(), serde_json::json!({"roles": [crate::infer_wire_tests::role(up.addr.port(), Some(free_port()))]}));
    let (st, _) = build(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
    let ProxyState::Listening(a) = *st.roles[0].proxy.lock().unwrap() else { panic!() };
    assert!(a.ip().is_loopback());
    // `expose` without a proxy port, or with a non-IP listen, is a config error.
    for bad in [
        serde_json::json!({"role":"a","flavor":"llamacpp","instance_port":9,"expose":{"listen":"0.0.0.0","port":7,"token_file":"t"}}),
        serde_json::json!({"role":"a","flavor":"llamacpp","instance_port":9,"proxy_port":8,"expose":{"listen":"all","port":7,"token_file":"t"}}),
        serde_json::json!({"role":"a","flavor":"llamacpp","instance_port":9,"proxy_port":8,"expose":{"listen":"0.0.0.0","port":7,"token_file":"../t"}}),
        serde_json::json!({"role":"a","flavor":"llamacpp","instance_port":9,"proxy_port":8,"expose":{"listen":"0.0.0.0","port":7,"token_file":"/etc/t"}}),
        serde_json::json!({"role":"a","flavor":"llamacpp","instance_port":9,"proxy_port":8,"expose":{"listen":"0.0.0.0","port":8,"token_file":"t"}}),
        serde_json::json!({"role":"a","flavor":"llamacpp","instance_port":9,"proxy_port":8,"expose":{"listen":"0.0.0.0","port":0,"token_file":"t"}}),
    ] {
        write_cfg(dir.path(), serde_json::json!({"roles": [bad]}));
        assert!(crate::infer_cfg::load_config(dir.path()).is_err());
    }
}

/// Run by `infer_wire_tests::operator_verbs_and_the_local_provider_hook`, not
/// as its own test: it calls [`init`], which fills process-global state (the
/// `infer.*` verb state, the placement role registry, the provider hook). Two
/// tests doing that in one test process race, and the verb test's "before
/// init" check only holds while nothing else has initialised.
pub(crate) async fn voice_and_the_llm_service_resolve_through_the_placed_roles() {
    let (hermes, orpheus) = (fake_on(0).await, fake_on(0).await);
    let dir = tempfile::tempdir().unwrap();
    write_cfg(
        dir.path(),
        serde_json::json!({"roles": [
            {"role": "hermes", "flavor": "llamacpp", "instance_port": hermes.addr.port(), "provider": "local"},
            {"role": "orpheus-tts", "flavor": "ollama", "instance_port": orpheus.addr.port()}]}),
    );
    let st = init(parts(dir.path(), None, false, None)).await.unwrap().unwrap();
    assert!(st.table.resolve("orpheus-tts").is_some());
    use clawft_types::placement::roles;
    // A role served here resolves straight to its server: no proxy hop.
    assert_eq!(roles::resolve("orpheus-tts"), Some(format!("http://127.0.0.1:{}", orpheus.addr.port())));
    assert_eq!(roles::resolve_for_provider("local"), Some(format!("http://127.0.0.1:{}", hermes.addr.port())));
    assert_eq!(roles::resolve("nothing"), None);
    // The LLM service client follows `local` unless an explicit URL was given.
    let resolved = crate::llm_service::resolve_llm_endpoint(None);
    if resolved.url_source.starts_with("placement") {
        assert_eq!(resolved.config.base_url, format!("http://127.0.0.1:{}", hermes.addr.port()));
    } else {
        assert!(resolved.url_source.starts_with("env") || resolved.url_source.starts_with("config") || resolved.url_source == "default:openrouter", "{}", resolved.url_source);
    }
}
