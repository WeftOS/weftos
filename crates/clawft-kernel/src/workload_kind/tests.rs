use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::workload_pkg::KIND_COG;

/// Test-only kind that accepts any body.
struct Echo;

impl WorkloadKind for Echo {
    fn id(&self) -> &'static str {
        "echo"
    }
    fn validate(&self, _: &ManifestEnvelope) -> Result<(), ManifestError> {
        Ok(())
    }
    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    fn load(
        &self,
        _: &Path,
        _: &TrustAnchors,
        _: &KindRegistry,
    ) -> Result<VerifiedWorkload, PlaneError> {
        Err(PlaneError::Package("echo has no package".into()))
    }
    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    fn spec(&self, _: &VerifiedWorkload) -> Result<WorkloadSpec, String> {
        Err("echo has no spec".into())
    }
    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    fn adapters(&self) -> &'static [&'static str] {
        &["native"]
    }
}

#[test]
fn builtin_registry_has_cog() {
    let r = KindRegistry::builtin();
    assert_eq!(r.ids(), vec![KIND_COG, "computer", "project"]);
    assert_eq!(r.get(KIND_COG).unwrap().id(), KIND_COG);
    assert!(r.require(KIND_COG).is_ok());
}

#[test]
fn unknown_kind_is_a_structured_error() {
    let r = KindRegistry::builtin();
    assert!(r.get("model").is_none());
    assert_eq!(r.require("model").err(), Some(UnknownKind("model".into())));
    let env = ManifestEnvelope::new("model", json!({}));
    let err = validate_envelope(&r, &env).unwrap_err();
    assert!(matches!(&err, ManifestError::UnknownKind(UnknownKind(k)) if k == "model"));
    assert_eq!(err.to_string(), "unknown workload kind \"model\"");
}

#[test]
fn registering_a_kind_makes_it_resolvable() {
    let mut r = KindRegistry::builtin();
    r.register(Arc::new(Echo)).unwrap();
    assert_eq!(r.ids(), vec![KIND_COG, "computer", "echo", "project"]);
    let env = ManifestEnvelope::new("echo", json!({"anything": 1}));
    assert!(validate_envelope(&r, &env).is_ok());
    // The builtin registry stays untouched (no global state).
    assert!(KindRegistry::builtin().get("echo").is_none());
}

#[test]
fn duplicate_ids_are_rejected() {
    let mut r = KindRegistry::builtin();
    assert_eq!(
        r.register(Arc::new(CogKind)).err(),
        Some(DuplicateKind(KIND_COG.into()))
    );
    r.register(Arc::new(Echo)).unwrap();
    assert!(r.register(Arc::new(Echo)).is_err());
}

#[test]
fn cog_kind_rejects_a_malformed_body_like_cog_body() {
    let r = KindRegistry::builtin();
    let env = ManifestEnvelope::new(KIND_COG, json!({"nope": true}));
    let via_registry = validate_envelope(&r, &env).unwrap_err().to_string();
    assert_eq!(via_registry, env.cog_body().unwrap_err().to_string());
}

#[test]
fn peek_reads_the_declared_kind() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(peek_manifest_kind(dir.path()), None);
    std::fs::write(
        dir.path().join(crate::workload_pkg::MANIFEST_FILE),
        r#"{"kind":"echo"}"#,
    )
    .unwrap();
    assert_eq!(peek_manifest_kind(dir.path()).as_deref(), Some("echo"));
    std::fs::write(
        dir.path().join(crate::workload_pkg::MANIFEST_FILE),
        "not json",
    )
    .unwrap();
    assert_eq!(peek_manifest_kind(dir.path()), None);
}

fn peek_in(manifest: &[u8]) -> Option<String> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(crate::workload_pkg::MANIFEST_FILE),
        manifest,
    )
    .unwrap();
    peek_manifest_kind(dir.path())
}

#[test]
fn peek_rejects_non_string_and_overlong_or_odd_kinds() {
    assert_eq!(peek_in(br#"{"kind":7}"#), None);
    assert_eq!(peek_in(br#"{"kind":null}"#), None);
    let long = format!(r#"{{"kind":"{}"}}"#, "a".repeat(MAX_KIND_LEN + 1));
    assert_eq!(peek_in(long.as_bytes()), None);
    let ok = format!(r#"{{"kind":"{}"}}"#, "a".repeat(MAX_KIND_LEN));
    assert!(peek_in(ok.as_bytes()).is_some());
    assert_eq!(peek_in(br#"{"kind":"has space"}"#), None);
    assert_eq!(peek_in(br#"{"kind":""}"#), None);
}

#[test]
fn peek_refuses_oversized_symlinked_and_fifo_manifests() {
    use crate::workload_pkg::{MANIFEST_FILE, MAX_MANIFEST_BYTES};
    let dir = tempfile::tempdir().unwrap();
    let m = dir.path().join(MANIFEST_FILE);
    let pad = " ".repeat(MAX_MANIFEST_BYTES + 1);
    std::fs::write(&m, format!(r#"{{"kind":"cog"}}{pad}"#)).unwrap();
    assert_eq!(peek_manifest_kind(dir.path()), None);

    std::fs::remove_file(&m).unwrap();
    let real = dir.path().join("real.json");
    std::fs::write(&real, r#"{"kind":"cog"}"#).unwrap();
    std::os::unix::fs::symlink(&real, &m).unwrap();
    assert_eq!(peek_manifest_kind(dir.path()), None);

    std::fs::remove_file(&m).unwrap();
    let c = std::ffi::CString::new(m.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    // Must return immediately rather than block opening the FIFO.
    assert_eq!(peek_manifest_kind(dir.path()), None);
}

#[test]
fn unknown_kind_text_is_truncated() {
    let r = KindRegistry::builtin();
    let e = r.require(&"x".repeat(500)).err().unwrap();
    assert_eq!(e.0.len(), MAX_KIND_LEN);
}
