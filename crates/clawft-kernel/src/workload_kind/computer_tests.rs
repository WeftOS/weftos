//! The `computer` kind: body validation, the D5 access rule, registration.

use serde_json::{Value, json};

use super::*;
use crate::workload_pkg::ManifestEnvelope;

fn own() -> Value {
    json!({"desktop": "xpc", "host_kind": "desktop",
           "capabilities": ["screen.capture", "input.type"], "access": "own"})
}
fn granted() -> Value {
    json!({"desktop": "macbook", "host_kind": "desktop",
           "capabilities": ["screen.capture", "input.click"], "access": "granted",
           "allowlist": {"apps": ["Terminal", "Google Chrome"], "displays": ["main"]}})
}
fn env(body: Value) -> ManifestEnvelope {
    ManifestEnvelope::new(KIND_COMPUTER, body)
}
fn with(mut body: Value, key: &str, v: Value) -> Value {
    body[key] = v;
    body
}

#[test]
fn registered_beside_cog_and_project_and_never_migratable() {
    let r = KindRegistry::builtin();
    assert!(r.ids().contains(&KIND_COMPUTER));
    let k = r.require(KIND_COMPUTER).unwrap();
    assert!(!k.migratable(), "a desktop's state is bound to its hardware");
    assert!(k.health().miss_limit >= 1);
    // Cog and project stay migratable / not, unchanged.
    assert!(r.get("cog").unwrap().migratable());
    assert!(!r.get("project").unwrap().migratable());
}

#[test]
fn valid_bodies_pass_validation_through_the_registry() {
    let r = KindRegistry::builtin();
    for body in [own(), granted()] {
        validate_envelope(&r, &env(body.clone())).unwrap_or_else(|e| panic!("{body}: {e}"));
    }
}

#[test]
fn bad_bodies_are_rejected_with_a_reason() {
    let r = KindRegistry::builtin();
    let cases: Vec<(Value, &str)> = vec![
        (json!({}), "computer body"),
        (json!("x"), "computer body"),
        (with(own(), "desktop", json!("")), "desktop"),
        (with(own(), "desktop", json!("Has Space")), "desktop"),
        (with(own(), "desktop", json!("-x")), "desktop"),
        (with(own(), "desktop", json!("x".repeat(65))), "desktop"),
        (with(own(), "host_kind", json!("mainframe")), "host_kind"),
        (with(own(), "capabilities", json!([])), "capabilities"),
        (with(own(), "capabilities", json!(["a", "a"])), "twice"),
        (with(own(), "capabilities", json!(["bad token"])), "capabilities[0]"),
        (with(own(), "access", json!("root")), "computer body"),
        (with(own(), "extra", json!(1)), "computer body"),
        // D5: own + allowlist contradicts; granted needs both non-empty lists.
        (with(own(), "allowlist", json!({"apps": ["a"], "displays": ["d"]})), "contradicts"),
        ({ let mut b = granted(); b.as_object_mut().unwrap().remove("allowlist"); b }, "needs an allowlist"),
        (with(granted(), "allowlist", json!({"apps": [], "displays": ["main"]})), "allowlist.apps"),
        (with(granted(), "allowlist", json!({"apps": ["a"], "displays": []})), "allowlist.displays"),
        (with(granted(), "allowlist", json!({"apps": ["a", "a"], "displays": ["d"]})), "twice"),
        (with(granted(), "allowlist", json!({"apps": ["a\u{0}"], "displays": ["d"]})), "printable"),
        (with(granted(), "allowlist", json!({"apps": [" a"], "displays": ["d"]})), "printable"),
        (with(granted(), "allowlist", json!({"apps": ["a"], "displays": ["d"], "windows": []})), "computer body"),
    ];
    for (body, needle) in cases {
        let e = validate_envelope(&r, &env(body.clone())).expect_err(&format!("{body}"));
        assert!(e.to_string().contains(needle), "{body}: {e} lacks {needle:?}");
    }
    // A wrong kind string on the envelope is refused by the kind itself.
    let wrong = ManifestEnvelope::new("cog", own());
    assert!(ComputerKind.validate(&wrong).is_err());
}

#[test]
fn allowlist_limits_are_enforced() {
    let many: Vec<String> = (0..=MAX_ALLOWLIST_TEST).map(|i| format!("app{i}")).collect();
    let body = with(granted(), "allowlist", json!({"apps": many, "displays": ["main"]}));
    assert!(ComputerBody::parse(&body).is_err());
}
const MAX_ALLOWLIST_TEST: usize = 64;

#[test]
fn d5_own_computer_is_full_access_without_an_ask() {
    let b = ComputerBody::parse(&own()).unwrap();
    for (app, display) in [("Terminal", "main"), ("anything", "other")] {
        assert_eq!(b.verdict(app, display), ComputerVerdict::Allow);
    }
}

#[test]
fn d5_granted_computer_asks_inside_the_allowlist_and_denies_outside_it() {
    let b = ComputerBody::parse(&granted()).unwrap();
    assert_eq!(b.verdict("Terminal", "main"), ComputerVerdict::Ask);
    assert_eq!(b.verdict("Google Chrome", "main"), ComputerVerdict::Ask);
    // Outside the allowlist is denied, not asked: app, display, or both.
    assert_eq!(b.verdict("Messages", "main"), ComputerVerdict::Deny);
    assert_eq!(b.verdict("Terminal", "external"), ComputerVerdict::Deny);
    assert_eq!(b.verdict("Messages", "external"), ComputerVerdict::Deny);
    // Matching is exact, never a prefix or case-fold.
    assert_eq!(b.verdict("terminal", "main"), ComputerVerdict::Deny);
    assert_eq!(b.verdict("Term", "main"), ComputerVerdict::Deny);
}

#[test]
fn body_round_trips_and_omits_an_absent_allowlist() {
    let b = ComputerBody::parse(&own()).unwrap();
    let v = serde_json::to_value(&b).unwrap();
    assert!(v.get("allowlist").is_none());
    assert_eq!(ComputerBody::parse(&v).unwrap(), b);
}

#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
mod runtime {
    use super::*;
    use crate::workload_runtime::WorkloadSource;

    #[test]
    fn a_valid_manifest_becomes_a_workload_pinned_to_its_desktop() {
        let w = computer_workload(&env(granted())).unwrap();
        assert_eq!((w.kind.as_str(), w.id.as_str()), ("computer", "macbook"));
        let WorkloadSource::Computer(c) = &w.source else { panic!("{w:?}") };
        assert_eq!(c.access, ComputerAccess::Granted);
        let spec = ComputerKind.spec(&w).unwrap();
        assert_eq!(spec.kind, "computer");
        assert_eq!(spec.requirements.common.len(), 1);
        assert!(matches!(
            &spec.requirements.common[0].selector,
            clawft_types::placement::IdSelector::Exact(id) if id.as_str() == "computer.desktop.macbook"
        ));
        assert_eq!(ComputerKind.adapters(), &["computer"]);
    }

    #[test]
    fn a_bad_manifest_never_becomes_a_workload() {
        assert!(computer_workload(&env(with(own(), "host_kind", json!("x")))).is_err());
        assert!(computer_workload(&ManifestEnvelope::new("cog", own())).is_err());
    }

    #[test]
    fn a_package_directory_is_refused_and_other_sources_are_not_computers() {
        let tmp = tempfile::tempdir().unwrap();
        let r = ComputerKind.load(tmp.path(), &crate::workload_pkg::TrustAnchors::default(), &KindRegistry::builtin());
        assert!(r.is_err());
        let cog_like = crate::workload_runtime::VerifiedWorkload {
            kind: "computer".into(),
            id: "x".into(),
            version: "1".into(),
            source: WorkloadSource::StorePin { registry: "r".into(), sha256: None },
        };
        assert!(ComputerKind.spec(&cog_like).is_err());
    }

    #[test]
    fn revocation_refs_name_the_computer() {
        let w = computer_workload(&env(own())).unwrap();
        let (pkg, keys, hashes) = w.revocation_refs();
        assert_eq!(pkg, "computer.xpc.1");
        assert!(keys.is_empty() && hashes.is_empty());
        assert!(w.signed("native").is_err());
    }
}
