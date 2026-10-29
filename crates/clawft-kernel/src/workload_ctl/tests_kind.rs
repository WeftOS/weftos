//! P2-B: the workload-kind registry on the placement path. The cog path
//! through the registry matches the pre-registry results.

use std::sync::Arc;

use ed25519_dalek::SigningKey;

use super::host_service::CtlConfig;
use super::plane::PlaneError;
use super::plane_place::PlaceOrder;
use super::test_support::*;
use super::transport::MeshConnector;
use crate::chain;
use crate::workload_kind::{CogKind, KindRegistry, WorkloadKind, validate_envelope};
use crate::workload_pkg::{
    DirSource, KIND_COG, MANIFEST_FILE, ManifestEnvelope, VerifyPolicy, verify_dir,
};
use crate::workload_runtime::{RunMode, VerifiedWorkload};

pub(super) fn order(pkg: &std::path::Path) -> PlaceOrder {
    PlaceOrder {
        package_dir: pkg.to_path_buf(),
        config: CtlConfig {
            mode: RunMode::Listener,
            args: vec![],
            csi_port: 15027,
        },
        pin: None,
        prefer: vec![],
        avoid: vec![],
        allow_emulated: false,
        start: true,
        dry_run: true,
        project_id: None,
    }
}

#[test]
fn cog_round_trip_through_the_registry_matches_direct_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "kind-cog", "#!/bin/sh\nexit 0\n", &[arch()]);
    let anchors = anchors();
    let env =
        ManifestEnvelope::from_bytes(&std::fs::read(pkg.join(MANIFEST_FILE)).unwrap()).unwrap();

    // Validation: same outcome as the old direct `cog_body` call.
    let reg = KindRegistry::builtin();
    assert!(validate_envelope(&reg, &env).is_ok() && env.cog_body().is_ok());

    // Load + spec: identical to the pre-registry sequence.
    let verified = verify_dir(&pkg, &anchors, &VerifyPolicy::default()).unwrap();
    let direct = VerifiedWorkload::from_package(&verified, &DirSource::new(&pkg)).unwrap();
    let direct_spec = super::cog_workload_spec(&direct).unwrap();
    let kind = reg.require(KIND_COG).unwrap();
    let via = kind.load(&pkg, &anchors, &reg).unwrap();
    assert_eq!(
        (&via.id, &via.kind, &via.version),
        (&direct.id, &direct.kind, &direct.version)
    );
    assert_eq!(
        serde_json::to_value(kind.spec(&via).unwrap()).unwrap(),
        serde_json::to_value(direct_spec).unwrap()
    );
    assert_eq!(CogKind.adapters(), kind.adapters());
}

#[tokio::test]
async fn unregistered_kind_is_refused_and_chained() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "kind-unknown", "#!/bin/sh\nexit 0\n", &[arch()]);
    let text = std::fs::read_to_string(pkg.join(MANIFEST_FILE)).unwrap();
    std::fs::write(
        pkg.join(MANIFEST_FILE),
        text.replace("\"kind\": \"cog\"", "\"kind\": \"model\""),
    )
    .unwrap();

    let (plane, chain_mgr) = controller(
        &SigningKey::from_bytes(&[41; 32]),
        Arc::new(MeshConnector::new(false)),
    );
    let err = plane.place(&order(&pkg)).await.unwrap_err();
    assert!(
        matches!(&err, PlaneError::UnknownKind(k) if k.0 == "model"),
        "{err:?}"
    );
    let refused = events(&chain_mgr, chain::EVENT_KIND_WORKLOAD_REFUSE);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].1["phase"], "kind");
}

#[tokio::test]
async fn a_registry_without_cog_refuses_cog_packages() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "kind-nocog", "#!/bin/sh\nexit 0\n", &[arch()]);
    let (plane, _c) = controller(
        &SigningKey::from_bytes(&[42; 32]),
        Arc::new(MeshConnector::new(false)),
    );
    let plane = plane.with_kind_registry(KindRegistry::new());
    let err = plane.place(&order(&pkg)).await.unwrap_err();
    assert!(matches!(err, PlaneError::UnknownKind(_)));
}

/// Test-only kind: a cog-shaped body under the id `echo`.
pub(super) struct Echo;

impl WorkloadKind for Echo {
    fn id(&self) -> &'static str {
        "echo"
    }
    fn validate(&self, env: &ManifestEnvelope) -> Result<(), crate::workload_pkg::ManifestError> {
        env.cog_shaped_body().map(|_| ())
    }
    fn load(
        &self,
        dir: &std::path::Path,
        anchors: &crate::workload_pkg::TrustAnchors,
        kinds: &KindRegistry,
    ) -> Result<VerifiedWorkload, PlaneError> {
        crate::workload_kind::load_cog_shaped(self.id(), dir, anchors, kinds)
    }
    fn spec(
        &self,
        w: &VerifiedWorkload,
    ) -> Result<clawft_types::placement::engine::WorkloadSpec, String> {
        super::cog_workload_spec(w)
    }
    fn adapters(&self) -> &'static [&'static str] {
        &["native"]
    }
}

/// A signed package re-labelled as kind `echo`.
pub(super) fn echo_package(root: &std::path::Path) -> std::path::PathBuf {
    use crate::workload_pkg::{key_id_for, sign_envelope, write_manifest};
    let pkg = package(root, "kind-echo", "#!/bin/sh\nexit 0\n", &[arch()]);
    let mut env =
        ManifestEnvelope::from_bytes(&std::fs::read(pkg.join(MANIFEST_FILE)).unwrap()).unwrap();
    env.kind = "echo".into();
    env.signatures.clear();
    let k = signer();
    sign_envelope(&mut env, &k, &key_id_for(&k.verifying_key().to_bytes())).unwrap();
    write_manifest(&pkg, &env).unwrap();
    pkg
}

#[test]
fn a_registered_echo_kind_prepares_and_seeds_through_the_plane() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = echo_package(tmp.path());
    let mut reg = KindRegistry::builtin();
    reg.register(Arc::new(Echo)).unwrap();
    let (plane, _c) = controller(
        &SigningKey::from_bytes(&[43; 32]),
        Arc::new(MeshConnector::new(false)),
    );
    let plane = plane.with_kind_registry(reg);
    let (w, spec, manifest_hash) = plane.prepare(&order(&pkg)).unwrap();
    assert_eq!((w.kind.as_str(), spec.kind.as_str()), ("echo", "echo"));
    assert_eq!(manifest_hash.len(), 64);
}

#[test]
fn echo_is_refused_by_the_default_registry_before_seeding() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = echo_package(tmp.path());
    let (plane, _c) = controller(
        &SigningKey::from_bytes(&[44; 32]),
        Arc::new(MeshConnector::new(false)),
    );
    let err = plane.prepare(&order(&pkg)).unwrap_err();
    // UnknownKind, not the Package error seeding or loading would give.
    assert!(matches!(err, PlaneError::UnknownKind(_)), "{err:?}");
    // The default seeding path refuses it too.
    assert!(plane.exchange.seed_package_dir(&pkg, &anchors()).is_err());
}

#[test]
fn a_kind_refuses_a_manifest_that_verifies_as_another_kind() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = echo_package(tmp.path());
    let mut reg = KindRegistry::builtin();
    reg.register(Arc::new(Echo)).unwrap();
    // The cog kind loading an echo package: the verified kind differs.
    let err = CogKind.load(&pkg, &anchors(), &reg).unwrap_err();
    assert!(
        matches!(&err, PlaneError::Package(m) if m.contains("is not \"cog\"")),
        "{err:?}"
    );
}

#[test]
fn project_sources_are_refused_by_signed() {
    use crate::workload_runtime::{ProjectPayload, WorkloadSource};
    let w = VerifiedWorkload {
        kind: "project".into(),
        id: "p".into(),
        version: "1".into(),
        source: WorkloadSource::Project(ProjectPayload {
            adapter: "logical".into(),
            project_id: "p".into(),
            key_id: "k".into(),
            cert_serial: 1,
            user_key_id: "u".into(),
            policy_hash: "00".into(),
            root: "/p".into(),
        }),
    };
    assert!(w.signed("native").is_err());
}

#[test]
fn macos_binary_package_yields_only_the_macos_native_route() {
    let tmp = tempfile::tempdir().unwrap();
    let pkg = package(tmp.path(), "mac-cog", "#!/bin/sh\nexit 0\n", &["aarch64-macos"]);
    let verified = verify_dir(&pkg, &anchors(), &VerifyPolicy::default()).unwrap();
    let w = VerifiedWorkload::from_package(&verified, &DirSource::new(&pkg)).unwrap();
    let spec = super::cog_workload_spec(&w).unwrap();
    let names: Vec<_> = spec.requirements.variants.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, ["aarch64-macos-native"]);
}
