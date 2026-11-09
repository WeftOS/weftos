//! The `project` kind, its permit and the `logical` adapter (ADR-103 A6,
//! Phase 2 package G).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use clawft_types::project::{CertRequest, ProjectCert};
use ed25519_dalek::SigningKey;
use serde_json::json;

use super::*;
use crate::gate::{GateBackend, GateDecision};
use crate::project_identity::{JournalRecord, RevocationView, sign_cert};
use crate::workload_governance::{
    NodeTrustTier, SUPERVISOR_PRINCIPAL, WorkloadGate, WorkloadPermitRule, project_supervisor_permit,
};
use crate::workload_pkg::{KIND_COG, ManifestEnvelope};
use crate::workload_runtime::{
    ChildIdentity, ChildLauncher, ChildProbe, ChildRef, ChildSpec, HostContract, LogicalRuntime, RunMode, RuntimeError,
    VerifiedWorkload, WorkloadConfig, WorkloadHost, WorkloadRuntime, WorkloadSource,
};

const ID: &str = "01JB8Z3Q0V6X9KQ4M2N7T5R1WD";

fn user_key() -> SigningKey {
    SigningKey::from_bytes(&[3u8; 32])
}

fn project_key() -> SigningKey {
    SigningKey::from_bytes(&[4u8; 32])
}

fn cert_for(id: &str, serial: u64) -> ProjectCert {
    sign_cert(
        &user_key(),
        &CertRequest {
            project_id: id.into(),
            project_pubkey: project_key().verifying_key().to_bytes(),
            serial,
            issued_at: Utc::now() - chrono::Duration::minutes(1),
            expires_at: None,
        },
    )
    .unwrap()
}

fn view(journal: &[JournalRecord], certs: &[ProjectCert]) -> RevocationView {
    RevocationView::build(&user_key().verifying_key().to_bytes(), &[], journal, certs)
}

fn facts<'a>(
    cert: Option<&'a ProjectCert>,
    v: &'a RevocationView,
    upub: &'a [u8; 32],
    root: &'a std::path::Path,
    manifest_id: &'a str,
) -> ProjectFacts<'a> {
    ProjectFacts { cert, user_pubkey: upub, revocations: v, manifest_id, root, policy_hash: "" }
}

#[test]
fn the_kind_is_builtin_has_no_package_and_runs_on_logical_only() {
    let r = KindRegistry::builtin();
    assert_eq!(r.ids(), vec![KIND_COG, "computer", KIND_PROJECT]);
    let k = r.require(KIND_PROJECT).unwrap();
    // `logical` (also fronting Seatbelt and container drivers) and the explicit Wasmtime adapter.
    assert_eq!(k.adapters(), ["logical", "wasmtime-project-v1"]);
    // No signed package, ever: a manifest claiming the kind is refused and
    // the package loader refuses too.
    assert!(k.validate(&ManifestEnvelope::new(KIND_PROJECT, json!({}))).is_err());
    let anchors = crate::workload_pkg::TrustAnchors::default();
    assert!(matches!(
        k.load(std::path::Path::new("/nonexistent"), &anchors, &r),
        Err(crate::workload_ctl::PlaneError::Package(m)) if m.contains("project.start")
    ));
}

#[test]
fn prepare_verifies_the_certificate_and_names_it() {
    let (upub, tmp) = (user_key().verifying_key().to_bytes(), tempfile::tempdir().unwrap());
    let cert = cert_for(ID, 2);
    let v = view(&[JournalRecord::Register { cert: cert.clone() }], &[]);
    let w = prepare_project(&facts(Some(&cert), &v, &upub, tmp.path(), ID)).unwrap();
    assert_eq!((w.kind.as_str(), w.id.as_str(), w.version.as_str()), ("project", ID, "cert-2"));
    let WorkloadSource::Project(p) = &w.source else { panic!("{w:?}") };
    assert_eq!((p.cert_serial, p.key_id.as_str()), (2, cert.project_key_id.as_str()));
    assert_eq!(p.root, tmp.path());
    // The spec asks for exactly the logical runtime.
    let spec = ProjectKind.spec(&w).unwrap();
    assert_eq!(spec.requirements.common.len(), 1);
}

#[test]
fn a_revoked_replaced_or_foreign_certificate_is_refused() {
    let (upub, tmp) = (user_key().verifying_key().to_bytes(), tempfile::tempdir().unwrap());
    let cert = cert_for(ID, 1);
    // Revoked key.
    let revoked = view(
        &[
            JournalRecord::Register { cert: cert.clone() },
            JournalRecord::Revoke { project_id: ID.into(), key_id: cert.project_key_id.clone() },
        ],
        &[],
    );
    assert!(matches!(
        prepare_project(&facts(Some(&cert), &revoked, &upub, tmp.path(), ID)),
        Err(ProjectPrepareError::Identity(_))
    ));
    // Signed by some other user key.
    let forged = sign_cert(
        &SigningKey::from_bytes(&[9u8; 32]),
        &CertRequest {
            project_id: ID.into(),
            project_pubkey: project_key().verifying_key().to_bytes(),
            serial: 1,
            issued_at: Utc::now() - chrono::Duration::minutes(1),
            expires_at: None,
        },
    )
    .unwrap();
    let v = view(&[], &[]);
    assert!(matches!(
        prepare_project(&facts(Some(&forged), &v, &upub, tmp.path(), ID)),
        Err(ProjectPrepareError::Cert(_))
    ));
    // A certificate for another project than the manifest's.
    let other = "01JB8Z3Q0V6X9KQ4M2N7T5R1WE";
    let theirs = cert_for(other, 1);
    let v = view(&[JournalRecord::Register { cert: theirs.clone() }], &[]);
    assert!(matches!(
        prepare_project(&facts(Some(&theirs), &v, &upub, tmp.path(), ID)),
        Err(ProjectPrepareError::WrongProject { .. })
    ));
    // A missing root.
    let v = view(&[], &[]);
    assert!(matches!(
        prepare_project(&facts(None, &v, &upub, &tmp.path().join("gone"), ID)),
        Err(ProjectPrepareError::RootGone(_))
    ));
}

#[test]
fn before_first_registration_the_workload_says_so() {
    let (upub, tmp) = (user_key().verifying_key().to_bytes(), tempfile::tempdir().unwrap());
    let v = view(&[], &[]);
    let w = prepare_project(&facts(None, &v, &upub, tmp.path(), ID)).unwrap();
    let WorkloadSource::Project(p) = &w.source else { panic!() };
    assert_eq!((p.key_id.as_str(), p.cert_serial, w.version.as_str()), ("unregistered", 0, "cert-0"));
}

fn ctx(kind: &str) -> serde_json::Value {
    json!({ "workload": {
        "kind": kind, "package_trust": "project_cert", "node_tier": "paired", "network": "none",
        "secrets": false, "emulated": false, "resource_cost": 0.25,
        "package_id": "project.x.1", "signer_keys": [], "artifact_hashes": [],
    }})
}

fn permitted(g: &WorkloadGate, who: &str, action: &str, kind: &str) -> bool {
    matches!(g.check(who, action, &ctx(kind)), GateDecision::Permit { .. })
}

#[test]
fn only_the_supervisor_principal_may_run_a_project() {
    // Default deny for everyone, supervisor included, without the permit.
    let bare = WorkloadGate::exempt(0.95, false, "test");
    for a in ["workload.load", "workload.start", "workload.stop"] {
        assert!(!permitted(&bare, SUPERVISOR_PRINCIPAL, a, "project"), "{a} must default-deny");
    }
    let g = WorkloadGate::exempt(0.95, false, "test").with_permit(project_supervisor_permit()).unwrap();
    for a in ["workload.load", "workload.start", "workload.stop", "workload.unload"] {
        assert!(permitted(&g, SUPERVISOR_PRINCIPAL, a, "project"), "{a}");
        assert!(!permitted(&g, "someone-else", a, "project"), "{a} for another principal");
        assert!(!permitted(&g, "", a, "project"), "{a} for no principal");
    }
    // The permit reaches nothing else: not placement, not other kinds.
    assert!(!permitted(&g, SUPERVISOR_PRINCIPAL, "workload.place", "project"));
    assert!(!permitted(&g, SUPERVISOR_PRINCIPAL, "workload.start", KIND_COG));
    // A signed-package trust label does not satisfy it either.
    let mut c = ctx("project");
    c["workload"]["package_trust"] = json!("unsigned");
    assert!(!matches!(g.check(SUPERVISOR_PRINCIPAL, "workload.start", &c), GateDecision::Permit { .. }));
}

#[test]
fn a_cog_permit_with_any_kind_does_not_accept_a_project_cert() {
    // Operator permits ask for a signed-package minimum; ProjectCert is below it.
    let mut any = WorkloadPermitRule::new("ANY", ["workload.start"], ["*"]);
    any.min_package_trust = crate::workload_governance::PackageTrust::OperatorAttested;
    let g = WorkloadGate::exempt(0.95, false, "test").with_permit(any).unwrap();
    assert!(!permitted(&g, "operator", "workload.start", "project"));
}

#[test]
fn permit_rules_with_principals_round_trip_and_validate() {
    let r = project_supervisor_permit();
    let back: WorkloadPermitRule = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
    assert_eq!(back, r);
    let mut bad = r.clone();
    bad.principals = vec![String::new()];
    assert!(bad.validate().is_err());
    // An old permit file (no `principals`) still parses and means "any".
    let old: WorkloadPermitRule = serde_json::from_value(json!({
        "id": "OLD", "actions": ["workload.start"], "kinds": ["cog"]
    }))
    .unwrap();
    assert!(old.principals.is_empty());
}

/// A launcher that records what it was asked and can lose the root.
#[derive(Default)]
struct Fake {
    log: Mutex<Vec<String>>,
    running: Mutex<bool>,
}

#[async_trait]
impl ChildLauncher for Fake {
    async fn spawn(&self, spec: &ChildSpec) -> Result<ChildRef, RuntimeError> {
        self.log.lock().unwrap().push(format!("spawn {}", spec.project_id));
        *self.running.lock().unwrap() = true;
        Ok(ChildRef { project_id: spec.project_id.clone(), identity: ChildIdentity::Native { host_pid: 4242 } })
    }
    async fn terminate(&self, c: &ChildRef, _g: Duration) -> Result<Option<i32>, RuntimeError> {
        self.log.lock().unwrap().push(format!("terminate {}", c.identity.host_pid()));
        *self.running.lock().unwrap() = false;
        Ok(Some(0))
    }
    async fn probe(&self, _id: &str) -> ChildProbe {
        if *self.running.lock().unwrap() { ChildProbe::Running { identity: ChildIdentity::Native { host_pid: 4242 } } } else { ChildProbe::NotStarted }
    }
}

fn workload(root: PathBuf) -> VerifiedWorkload {
    let (upub, v) = (user_key().verifying_key().to_bytes(), view(&[], &[]));
    let mut w = prepare_project(&facts(None, &v, &upub, &root, ID)).unwrap();
    let _ = &mut w;
    w
}

fn cfg() -> WorkloadConfig {
    WorkloadConfig {
        mode: RunMode::Listener,
        args: Vec::new(),
        host: HostContract::default_feed(),
        node_id: ID.into(),
    }
}

#[tokio::test]
async fn logical_admission_fails_when_the_root_vanished() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let rt = LogicalRuntime::new(Arc::new(Fake::default()));
    let w = workload(root.clone());
    assert!(rt.admit(&w).await.is_ok());
    std::fs::remove_dir_all(&root).unwrap();
    let err = rt.admit(&w).await.unwrap_err();
    assert!(matches!(err, RuntimeError::AdmissionRefused(m) if m.contains("is gone")));
    // And it refuses anything that is not a project.
    let mut cog = w.clone();
    cog.source = WorkloadSource::StorePin { registry: "r".into(), sha256: None };
    assert!(matches!(rt.admit(&cog).await, Err(RuntimeError::AdmissionRefused(_))));
}

#[tokio::test]
async fn the_host_runs_a_project_end_to_end_only_for_the_supervisor() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let fake = Arc::new(Fake::default());
    let gate: Arc<dyn GateBackend> =
        Arc::new(WorkloadGate::exempt(0.95, false, "test").with_permit(project_supervisor_permit()).unwrap());
    let host = WorkloadHost::new(
        Arc::new(LogicalRuntime::new(fake.clone())),
        gate.clone(),
        SUPERVISOR_PRINCIPAL,
        NodeTrustTier::Paired,
    );
    let w = workload(root.clone());
    let h = host.load(&w, &cfg()).await.unwrap();
    host.start(&h).await.unwrap();
    assert!(host.start(&h).await.is_err(), "no second child for one project");
    assert_eq!(host.status(&h).await.state, crate::workload_runtime::InstanceState::Running);
    host.stop(&h, Duration::from_millis(10)).await.unwrap();
    host.unload(h).await.unwrap();
    assert_eq!(*fake.log.lock().unwrap(), ["spawn 01JB8Z3Q0V6X9KQ4M2N7T5R1WD", "terminate 4242"]);

    // The same adapter under any other principal is denied at load.
    let other = WorkloadHost::new(
        Arc::new(LogicalRuntime::new(Arc::new(Fake::default()))),
        gate,
        "mallory",
        NodeTrustTier::Paired,
    );
    let err = other.load(&w, &cfg()).await.unwrap_err();
    assert!(matches!(err, RuntimeError::Governance(_)), "{err:?}");
}

#[tokio::test]
async fn unload_refuses_while_the_child_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let rt = LogicalRuntime::new(Arc::new(Fake::default()));
    let w = workload(tmp.path().to_path_buf());
    let h = rt.load(&w, &cfg()).await.unwrap();
    rt.start(&h).await.unwrap();
    assert!(matches!(rt.unload(h.clone()).await, Err(RuntimeError::InvalidState(_))));
    rt.stop(&h, Duration::from_millis(1)).await.unwrap();
    rt.unload(h).await.unwrap();
}

#[test]
fn the_spec_requires_the_capability_of_the_selected_adapter_and_refuses_unknown_ones() {
    let (upub, tmp) = (user_key().verifying_key().to_bytes(), tempfile::tempdir().unwrap());
    let cert = cert_for(ID, 2);
    let v = view(&[JournalRecord::Register { cert: cert.clone() }], &[]);
    let mut w = prepare_project(&facts(Some(&cert), &v, &upub, tmp.path(), ID)).unwrap();
    let WorkloadSource::Project(p) = &mut w.source else { panic!("{w:?}") };
    assert_eq!(p.adapter, "logical", "prepare_project defaults to the native adapter");
    p.adapter = "wasmtime-project-v1".into();
    let spec = ProjectKind.spec(&w).unwrap();
    assert!(format!("{:?}", spec.requirements.common).contains("runtime.project.wasmtime"));
    for bad in ["", "wasmtime", "wasmtime-project-v2", "container"] {
        let WorkloadSource::Project(p) = &mut w.source else { unreachable!() };
        p.adapter = bad.into();
        assert!(ProjectKind.spec(&w).is_err(), "adapter {bad:?} must be refused");
    }
}
