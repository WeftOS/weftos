//! `project.fetch` end to end over the real signed `workload.ctl` wire between
//! two in-process nodes: the primary (serving the project, trusting only its
//! own key as controller) and a member (paired, with a fetch grant). Temp dirs
//! only; nothing touches the real `~/.weftos`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use clawft_kernel::chain::ChainManager;
use clawft_kernel::node_facts_advert::sign_node_facts;
use clawft_kernel::workload_ctl::msg::method;
use clawft_kernel::workload_ctl::{MeshConnector, NodeAdmin, PlacementControlPlane, WorkloadHostService};
use clawft_kernel::workload_governance::WorkloadGate;
use clawft_kernel::workload_pkg::TrustAnchors;
use clawft_kernel::{ArtifactExchange, ArtifactStore, ExchangeConfig, node_id_from_pubkey};
use clawft_types::placement::{NodeFacts, TrustTier};
use clawft_types::project::ProjectManifest;
use ed25519_dalek::SigningKey;
use serde_json::{Value, json};

use super::*;
use crate::mesh_names::MeshNames;
use crate::project_fetch_client::{self as client, BundleMode, FetchChannel, PlaneChannel};
use crate::project_fetch_grants::FETCH_FILE as GRANTS_FILE;
use crate::weftos_uri::WeftosUri;
use crate::project_fetch_policy::FetchPeerPolicy;
use crate::project_fetch_repos::git;
use crate::project_fetch_serve::FetchHost;
use crate::project_install::{InstallRequest, PrimaryRef, ProjectFetcher};

pub(crate) const ULID: &str = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1C";
/// The member's mesh (the authority its clones are named on).
pub(crate) const MESH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The member's mesh names for a fetcher under test.
pub(crate) fn mesh_names() -> MeshNames {
    let mut id = [0u8; 32];
    hex::decode_to_slice(MESH, &mut id).unwrap();
    MeshNames { mesh: Some(id) }
}
const OTHER: &str = "01K6ZQ8N3T4V5W6X7Y8Z9A0B1D";
const T: Duration = Duration::from_secs(30);

pub(crate) fn exchange(id: &str, chain: &Arc<ChainManager>) -> Arc<ArtifactExchange> {
    let mut ex = ArtifactExchange::new(id, Arc::new(ArtifactStore::new_memory()), ExchangeConfig::default()).unwrap();
    ex.set_chain_manager(chain.clone());
    Arc::new(ex)
}

pub(crate) fn gate(chain: &Arc<ChainManager>) -> Arc<WorkloadGate> {
    Arc::new(WorkloadGate::exempt(0.95, false, "test").with_chain(chain.clone()))
}

pub(crate) fn w(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

pub(crate) fn g(repo: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&git(repo, args, None, T).unwrap()).trim().to_owned()
}

pub(crate) fn commit(repo: &Path, msg: &str) -> String {
    g(repo, &["add", "-A"]);
    g(repo, &["-c", "user.name=t", "-c", "user.email=t@example.invalid", "commit", "-q", "-m", msg]);
    g(repo, &["rev-parse", "HEAD"])
}

/// The hook the primary installs: `project.fetch` only.
struct Admin(Arc<FetchHost>);

#[async_trait]
impl NodeAdmin for Admin {
    async fn call(&self, m: &str, requester: &str, body: &Value) -> Result<Value, String> {
        self.call_raw(m, requester, body).await.map(|(v, _)| v)
    }
    async fn call_raw(&self, m: &str, requester: &str, body: &Value) -> Result<(Value, Option<Vec<u8>>), String> {
        if m != method::PROJECT_FETCH {
            return Err(format!("{m} is not served"));
        }
        self.0.serve_raw(requester, body).await
    }
}

pub(crate) struct Rig {
    pub(crate) tmp: tempfile::TempDir,
    /// Primary runtime dir (peer + grant files).
    pub(crate) rt: PathBuf,
    pub(crate) root: PathBuf,
    pub(crate) host_id: String,
    pub(crate) member_key: SigningKey,
    pub(crate) host_chain: Arc<ChainManager>,
    pub(crate) member_chain: Arc<ChainManager>,
    pub(crate) fetch_host: Arc<FetchHost>,
    pub(crate) addr: String,
    pub(crate) conn: Arc<MeshConnector>,
}

impl Rig {
    pub(crate) fn peers(&self, tier: &str) {
        let k = hex::encode(self.member_key.verifying_key().to_bytes());
        w(&self.rt.join("workload-peers.json"), &format!("[{{\"addr\":\"127.0.0.1:9471\",\"tier\":\"{tier}\",\"key\":\"{k}\"}}]"));
    }

    pub(crate) fn grant(&self, projects: &[&str]) {
        let member = node_id_from_pubkey(&self.member_key.verifying_key().to_bytes());
        let p: Vec<String> = projects.iter().map(|s| format!("\"{s}\"")).collect();
        w(&self.rt.join(GRANTS_FILE), &format!("{{\"version\":1,\"grants\":[{{\"peer_node\":\"{member}\",\"projects\":[{}]}}]}}", p.join(",")));
    }

    fn revoke(&self) {
        std::fs::remove_file(self.rt.join(GRANTS_FILE)).unwrap();
    }

    /// The member's control plane; learning the primary is a signed `describe`.
    pub(crate) async fn plane(&self) -> Result<Arc<PlacementControlPlane>, String> {
        let id = node_id_from_pubkey(&self.member_key.verifying_key().to_bytes());
        let plane = PlacementControlPlane::new(self.member_key.clone(), gate(&self.member_chain), self.member_chain.clone(), exchange(&id, &self.member_chain), TrustAnchors::default(), self.conn.clone());
        plane.add_target(&self.addr, TrustTier::Pinned).await.map_err(|e| e.to_string())?;
        Ok(Arc::new(plane))
    }

    /// The reason the primary refused to be learned.
    async fn plane_refused(&self) -> String {
        match self.plane().await {
            Err(e) => e,
            Ok(_) => panic!("the primary answered a peer it should refuse"),
        }
    }

    async fn channel(&self) -> PlaneChannel {
        PlaneChannel::new(self.plane().await.unwrap(), self.host_id.clone())
    }

    pub(crate) fn dest(&self) -> PathBuf {
        self.tmp.path().join("member").join("Projects").join("demo")
    }
}

/// A primary holding one project: a root repository with `README`, an ignored
/// `data/` tree, a sibling-style sub-repository `tools/`, an archived
/// `models/`, a `.env`, and a symlink out of the project.
pub(crate) async fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let rt = tmp.path().join("primary-rt");
    let manifests = tmp.path().join("primary-manifests");
    let root = tmp.path().join("primary").join("demo");
    std::fs::create_dir_all(&rt).unwrap();
    w(&root.join("README.md"), "# demo\n");
    // Archived paths are kept out of git on the primary (D-C); git content
    // travels as git, whatever the archive list says.
    w(&root.join(".gitignore"), "data/\n.env\n.weftos/\nescape\nmodels/\n");
    w(&root.join("data/set.csv"), "csv-sentinel-q7x\n");
    w(&root.join(".env"), "TOKEN=nope\n");
    w(&root.join(".weftos/project.toml"), "id = \"x\"\n");
    w(&root.join(".weftos/archive.toml"), "[[archive]]\npath = \"models\"\nreason = \"weights\"\n");
    w(&root.join("models/w.bin"), "weights");
    w(&tmp.path().join("outside.txt"), "outside");
    std::os::unix::fs::symlink(tmp.path().join("outside.txt"), root.join("escape")).unwrap();
    g(&root, &["init", "-q", "-b", "main"]);
    commit(&root, "init");
    let tools = root.join("tools");
    w(&tools.join("run.sh"), "echo hi\n");
    g(&tools, &["init", "-q", "-b", "main"]);
    commit(&tools, "tools");
    let now = chrono::Utc::now();
    let m = ProjectManifest {
        schema_version: 1,
        id: ULID.into(),
        name: "demo".into(),
        root: root.clone(),
        state: Default::default(),
        created: now,
        last_seen: now,
        project_toml: Default::default(),
        seed: None,
        legacy: None,
        serve: None,
        chain: None,
        binary: None,
        extra: toml::Table::new(),
    };
    clawft_types::project::write_manifest(&manifests, &m).unwrap();

    let host_key = SigningKey::from_bytes(&[81; 32]);
    let member_key = SigningKey::from_bytes(&[82; 32]);
    let host_pk = host_key.verifying_key().to_bytes();
    let host_id = node_id_from_pubkey(&host_pk);
    let host_chain = Arc::new(ChainManager::new(0, 1000));
    let fetch = Arc::new(FetchHost::new(rt.clone(), manifests).with_chain(host_chain.clone()));
    let svc = WorkloadHostService::new(host_key.clone(), exchange(&host_id, &host_chain), TrustAnchors::default(), gate(&host_chain))
        .with_controllers(FetchPeerPolicy::new(vec![host_pk], rt.clone()))
        .with_chain(host_chain.clone());
    let now_s = chrono::Utc::now().timestamp() as u64;
    svc.set_facts(sign_node_facts(&NodeFacts::new(host_id.clone(), now_s, 600, 1), &host_key).unwrap());
    assert!(svc.set_node_admin(Arc::new(Admin(fetch.clone()))));
    let conn = Arc::new(MeshConnector::new(false));
    let addr = conn.register_local("primary", Arc::new(svc));
    Rig { tmp, rt, root, host_id, member_key, host_chain, member_chain: Arc::new(ChainManager::new(0, 1000)), fetch_host: fetch, addr, conn }
}

fn kinds(c: &ChainManager, kind: &str) -> Vec<Value> {
    c.tail(c.len()).into_iter().filter(|e| e.kind == kind).filter_map(|e| e.payload).collect()
}

fn request(r: &Rig) -> InstallRequest {
    InstallRequest {
        project_ulid: ULID.into(),
        target_path: Some(r.dest().to_string_lossy().into_owned()),
        slug: None,
        sources: vec![],
        primary: Some(PrimaryRef { node_id: r.host_id.clone() }),
    }
}

#[tokio::test]
async fn a_paired_member_clones_every_repository_and_the_non_git_content_then_pulls_incrementally() {
    let r = rig().await;
    r.peers("pinned");
    r.grant(&[ULID]);
    let plane = r.plane().await.unwrap();
    let f = MeshFetcher::with_plane(plane.clone(), Some(r.member_chain.clone())).with_mesh(mesh_names());
    let req = request(&r);
    assert!(f.can_fetch(&req));
    let dest = r.dest();
    let rep = f.fetch(&req, &dest).await.unwrap();

    assert_eq!(rep.fetcher, "mesh");
    let dirs: Vec<&str> = rep.repos.iter().map(|x| x.dir.as_str()).collect();
    assert_eq!(dirs, vec![".", "tools"]);
    assert!(rep.repos[0].head.is_some());
    // Clones are named on the mesh, not on the node that served them (ADR-114).
    assert_eq!(rep.repos[0].remote.as_deref(), Some(format!("weftos://{MESH}/projects/{ULID}").as_str()));
    assert_eq!(rep.repos[1].remote.as_deref(), Some(format!("weftos://{MESH}/projects/{ULID}/repos/tools").as_str()));
    assert_eq!(std::fs::read_to_string(dest.join("README.md")).unwrap(), "# demo\n");
    assert_eq!(g(&dest, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(g(&dest, &["config", "remote.origin.url"]), format!("weftos://{MESH}/projects/{ULID}"));
    assert_eq!(WeftosUri::parse(&g(&dest, &["config", "remote.origin.url"])).unwrap().project_repo(), Some((ULID, ".")));
    // The sibling layout: `tools` next to the root checkout.
    assert!(dest.parent().unwrap().join("tools/run.sh").exists());
    // Non-git content arrived; what must stay home stayed home.
    assert_eq!(std::fs::read_to_string(dest.join("data/set.csv")).unwrap(), "csv-sentinel-q7x\n");
    assert!(rep.bytes > 0);
    assert!(!dest.join(".env").exists(), ".env never leaves the primary");
    assert!(std::fs::symlink_metadata(dest.join("escape")).is_err(), "a symlink out of the project is never sent");
    assert!(!dest.join("models").exists(), "an archived path is never sent");
    assert_eq!(rep.archived, vec!["models (weights)"]);
    assert!(!dest.join(".weftos/project.toml").exists());
    assert!(rep.warnings.iter().any(|x| x.contains("stayed on the primary")), "{:?}", rep.warnings);

    // Chained on both nodes, without content.
    let host = kinds(&r.host_chain, "project.fetch");
    assert!(host.iter().any(|p| p["op"] == "bundle.open" && p["ok"] == true && p["project"] == ULID));
    assert!(host.iter().any(|p| p["op"] == "tar.open" && p["ok"] == true));
    let member = kinds(&r.member_chain, "project.fetch");
    assert_eq!(member.len(), 1);
    assert_eq!((member[0]["ok"].clone(), member[0]["repos"].clone()), (json!(true), json!(2)));
    let all = serde_json::to_string(&r.host_chain.tail(r.host_chain.len())).unwrap();
    assert!(!all.contains("TOKEN=nope") && !all.contains("csv-sentinel-q7x"));

    // Incremental: a new commit on the primary travels as a bundle that
    // excludes what the member has; up to date means nothing is sent.
    let old = g(&dest, &["rev-parse", "HEAD"]);
    w(&r.root.join("new.txt"), "new\n");
    let new = commit(&r.root, "second");
    let ch = PlaneChannel::new(plane, r.host_id.clone());
    let gitdir = dest.join(".git");
    let want = vec!["refs/heads/main".to_string()];
    let bytes = client::fetch_bundle(&ch, ULID, ".", &want, std::slice::from_ref(&old), &gitdir, BundleMode::Unbundle).await.unwrap();
    assert!(bytes.is_some());
    g(&dest, &["cat-file", "-e", &new]);
    assert!(client::fetch_bundle(&ch, ULID, ".", &want, std::slice::from_ref(&new), &gitdir, BundleMode::Unbundle).await.unwrap().is_none(), "nothing to send");
    // The incremental bundle really excluded the old history: unbundling it
    // into a repository that lacks the `have` fails on the missing prerequisite.
    let fresh = tempfile::tempdir().unwrap();
    g(fresh.path(), &["init", "-q"]);
    let e = client::fetch_bundle(&ch, ULID, ".", &want, &[old], fresh.path(), BundleMode::Unbundle).await.unwrap_err();
    assert!(e.contains("git bundle"), "{e}");
}

#[tokio::test]
async fn an_unlisted_peer_cannot_even_describe_the_primary() {
    let r = rig().await;
    r.grant(&[ULID]); // a grant without a peer entry
    let e = r.plane_refused().await;
    assert!(e.contains("unauthorized"), "{e}");
    assert!(kinds(&r.host_chain, "project.fetch").is_empty());
}

#[tokio::test]
async fn a_discovered_peer_or_one_without_a_key_is_refused() {
    let r = rig().await;
    r.grant(&[ULID]);
    r.peers("discovered");
    assert!(r.plane_refused().await.contains("unauthorized"));
    w(&r.rt.join("workload-peers.json"), r#"[{"addr":"127.0.0.1:9471","tier":"pinned"}]"#);
    assert!(r.plane_refused().await.contains("unauthorized"));
}

#[tokio::test]
async fn a_revoked_peer_is_stopped_on_the_next_call_and_mid_transfer() {
    let r = rig().await;
    r.peers("paired");
    r.grant(&[ULID]);
    let ch = r.channel().await;
    client::list(&ch, ULID).await.unwrap();
    let opened = ch.call(json!({"op": "bundle.open", "project": ULID, "dir": ".", "want": ["refs/heads/main"]})).await.unwrap();
    let session = opened["session"].as_str().unwrap().to_owned();
    // The grant for this project is withdrawn (another one remains, so the
    // key still reaches the hook): the hook refuses and drops the transfer.
    r.grant(&[OTHER]);
    let e = client::list(&ch, ULID).await.unwrap_err();
    assert!(e.contains("no fetch grant"), "{e}");
    let e = ch.call(json!({"op": "chunk", "session": session, "index": 0})).await.unwrap_err();
    assert!(e.contains("no fetch grant"), "{e}");
    // The session is gone, not merely paused.
    r.grant(&[ULID]);
    let e = ch.call(json!({"op": "chunk", "session": session, "index": 0})).await.unwrap_err();
    assert!(e.contains("no such transfer"), "{e}");
    let refused = kinds(&r.host_chain, "project.fetch").into_iter().filter(|p| p["ok"] == false).count();
    assert!(refused >= 2, "refusals are chained");
    // With no grant at all the key is refused before the hook (as a stranger).
    r.revoke();
    let e = client::list(&ch, ULID).await.unwrap_err();
    assert!(e.contains("unauthorized"), "{e}");
    // And once the peer entry itself is gone, the same.
    r.grant(&[ULID]);
    let plane = r.plane().await.unwrap();
    std::fs::remove_file(r.rt.join("workload-peers.json")).unwrap();
    let f = MeshFetcher::with_plane(plane, None).with_mesh(mesh_names());
    assert!(f.can_fetch(&request(&r)), "the member's own view still lists the primary");
    let e = f.fetch(&request(&r), &r.dest()).await.unwrap_err();
    assert!(e.contains("unauthorized"), "the primary no longer admits the key at all: {e}");
}

#[tokio::test]
async fn another_projects_ulid_and_unknown_projects_are_refused() {
    let r = rig().await;
    r.peers("paired");
    r.grant(&[OTHER]);
    let ch = r.channel().await;
    let e = client::list(&ch, ULID).await.unwrap_err();
    assert!(e.contains("no fetch grant"), "{e}");
    // Granted but not held here.
    let e = client::list(&ch, OTHER).await.unwrap_err();
    assert!(e.contains("does not hold"), "{e}");
    let e = ch.call(json!({"op": "list", "project": "nope"})).await.unwrap_err();
    assert!(e.contains("ULID"), "{e}");
    let e = ch.call(json!({"op": "list", "project": ULID, "extra": 1})).await.unwrap_err();
    assert!(e.contains("unknown field"), "{e}");
}

#[tokio::test]
async fn paths_outside_the_project_are_refused() {
    let r = rig().await;
    r.peers("paired");
    r.grant(&[ULID]);
    let ch = r.channel().await;
    for dir in ["../primary", "..", "/etc", "a/b", ".weftos", ".git"] {
        let e = client::refs(&ch, ULID, dir).await.unwrap_err();
        assert!(e.contains("dir must be") || e.contains("no such repository"), "{dir}: {e}");
    }
    assert!(client::refs(&ch, ULID, "nope").await.unwrap_err().contains("no such repository"));
    let e = ch.call(json!({"op": "bundle.open", "project": ULID, "dir": ".", "want": ["--output=/tmp/x"]})).await.unwrap_err();
    assert!(e.contains("not a branch or tag"), "{e}");
    let e = ch.call(json!({"op": "bundle.open", "project": ULID, "dir": ".", "want": ["refs/heads/main"], "have": ["../../x"]})).await.unwrap_err();
    assert!(e.contains("object ids"), "{e}");
    assert!(WeftosUri::parse(&format!("weftos://{MESH}/projects/{ULID}/repos/../x")).is_err());
    assert_eq!(WeftosUri::parse(&format!("weftos://{MESH}/projects/{ULID}/repos/.weftos")).unwrap().project_repo(), None);
}

#[tokio::test]
async fn a_workspace_copy_never_serves_and_a_second_project_is_invisible() {
    let r = rig().await;
    r.peers("paired");
    r.grant(&[ULID, OTHER]);
    let ch = r.channel().await;
    let listing = client::list(&ch, ULID).await.unwrap();
    assert_eq!(listing["repos"].as_array().unwrap().len(), 2);
    assert_eq!(listing["archived"], json!(["models (weights)"]));
    assert_eq!(listing["nongit"]["files"], 1);
    assert_eq!(listing["nongit"]["excluded"], 2, ".env and the symlink");
    // Turn the manifest into a workspace: it is a copy, not the primary.
    let manifests = r.tmp.path().join("primary-manifests");
    let mut m = clawft_types::project::find_by_id(&manifests, ULID).unwrap().unwrap();
    m.extra.insert("role".into(), toml::Value::String("workspace".into()));
    clawft_types::project::write_manifest(&manifests, &m).unwrap();
    assert!(client::list(&ch, ULID).await.unwrap_err().contains("workspace copy"));
}

#[test]
fn names_and_layout() {
    let u = WeftosUri::parse(&format!("weftos://{MESH}/projects/{ULID}/repos/tools")).unwrap();
    assert_eq!(u.project_repo(), Some((ULID, "tools")));
    assert_eq!(WeftosUri::parse(&format!("weftos://{MESH}/projects/{ULID}")).unwrap().project_repo(), Some((ULID, ".")));
    // The old node-addressed form and anything outside the grammar are not names.
    for bad in ["https://x/y", "weftos:///x", &format!("weftos://n/{ULID}/."), &format!("weftos://n/{ULID}/tools"), &format!("weftos://n/projects/{ULID}/"), &format!("weftos://n o/projects/{ULID}")] {
        assert!(WeftosUri::parse(bad).is_err(), "{bad}");
    }
    let t = Path::new("/home/m/Projects/demo");
    assert_eq!(layout(t, true, "."), t);
    assert_eq!(layout(t, true, "tools"), Path::new("/home/m/Projects/tools"));
    assert_eq!(layout(t, false, "tools"), Path::new("/home/m/Projects/demo/tools"));
}

#[test]
fn can_fetch_needs_a_named_primary() {
    let chain = Arc::new(ChainManager::new(0, 1000));
    let key = SigningKey::from_bytes(&[83; 32]);
    let id = node_id_from_pubkey(&key.verifying_key().to_bytes());
    let plane = Arc::new(PlacementControlPlane::new(key, gate(&chain), chain.clone(), exchange(&id, &chain), TrustAnchors::default(), Arc::new(MeshConnector::new(false))));
    let f = MeshFetcher::with_plane(plane, None);
    let mut req = InstallRequest { project_ulid: ULID.into(), target_path: None, slug: Some("demo".into()), sources: vec![], primary: None };
    assert!(!f.can_fetch(&req));
    req.primary = Some(PrimaryRef { node_id: "nobody".into() });
    assert!(!f.can_fetch(&req), "an unknown node is not a paired peer");
    assert_eq!(f.name(), "mesh");
}
