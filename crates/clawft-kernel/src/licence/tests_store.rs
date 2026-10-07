//! Stores: binding, grants, floor, approvals, persistence.

use std::sync::Arc;

use super::tests_common::*;
use super::*;
use crate::revocation::{RevocationKind, RevocationList};

const HOUR: u64 = 3600;
const DAY: u64 = 86_400;

fn put(fx: &Fx, g: &SignedGrant) -> Result<Outcome, LicenceError> {
    fx.store.accept_grant(g)
}

fn covered(fx: &Fx, arch: &str) -> bool {
    fx.store.valid_grant_covering(&b3_of(arch), "fall-detect", "1.2.0").is_some()
}

// ── binding ──────────────────────────────────────────────────────

#[test]
fn no_binding_means_nothing_is_in_effect() {
    let fx = Fx::new();
    assert!(fx.store.active_binding().is_none());
    assert_eq!(fx.store.binding_status(), Err(LicenceError::NoBinding));
    assert_eq!(put(&fx, &grant(1, T0, HOUR, &["aarch64"])), Err(LicenceError::NoBinding));
}

#[test]
fn open_membership_observe_and_off_refuse_the_binding() {
    let fx = Fx::new();
    let cases = [
        (AdmissionPosture { open_membership: true, ..posture() }, "open_membership"),
        (AdmissionPosture { enforce: false, ..posture() }, "admission_not_enforce"),
        (AdmissionPosture { verdict_source_bound: false, ..posture() }, "no_verdict_source"),
    ];
    for (p, why) in cases {
        let r = fx.store.accept_binding(&binding(1, BindState::Bound), p, &NoExtraChecks);
        assert_eq!(r, Err(LicenceError::BindingRefused(why)));
    }
    assert!(fx.store.active_binding().is_none());
    assert_eq!(fx.names(), ["binding_refused"; 3]);
}

#[test]
fn binding_seq_rules_and_conflict() {
    let fx = Fx::new();
    let s = |n, st| binding(n, st);
    let ok = |b: &SignedBinding| fx.store.accept_binding(b, posture(), &NoExtraChecks);
    assert_eq!(ok(&s(5, BindState::Bound)), Ok(Outcome::Applied));
    assert_eq!(ok(&s(5, BindState::Bound)), Ok(Outcome::Duplicate));
    assert_eq!(ok(&s(4, BindState::Unbound)), Ok(Outcome::Ignored));
    assert!(fx.store.active_binding().is_some());
    // A different record at the same seq is refused and chained.
    let mut other = binding_rec(5, BindState::Bound, &grant_key(), &mesh());
    other.steward_node_id = "node-other".into();
    let other = sign_binding(&other, &op()).unwrap();
    assert_eq!(ok(&other), Err(LicenceError::Conflict(5)));
    assert_eq!(fx.names(), ["binding_conflict"]);
}

#[test]
fn binding_for_another_mesh_or_unpinned_signer_is_refused_by_the_store() {
    let fx = Fx::new();
    let wrong = sign_binding(&binding_rec(1, BindState::Bound, &grant_key(), &other_mesh()), &op())
        .unwrap();
    let ok = |b| fx.store.accept_binding(b, posture(), &NoExtraChecks);
    assert_eq!(ok(&wrong), Err(LicenceError::WrongMesh));
    let rogue = sign_binding(&binding_rec(1, BindState::Bound, &grant_key(), &mesh()), &sk(33))
        .unwrap();
    assert_eq!(ok(&rogue), Err(LicenceError::UntrustedKey));
}

#[test]
fn steward_hook_runs_after_member_checks() {
    struct Deny;
    impl BindingExtraCheck for Deny {
        fn check(&self, _: &BindingRecord) -> Result<(), LicenceError> {
            Err(LicenceError::CheckFailed("fingerprint".into()))
        }
    }
    let fx = Fx::new();
    let r = fx.store.accept_binding(&binding(1, BindState::Bound), posture(), &Deny);
    assert_eq!(r, Err(LicenceError::CheckFailed("fingerprint".into())));
    assert!(fx.store.active_binding().is_none());
}

#[test]
fn unbind_stops_grants_at_once_and_a_same_key_rebind_resumes_them() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, DAY, &["aarch64"])).unwrap();
    assert!(covered(&fx, "aarch64"));
    let unbind = binding(2, BindState::Unbound);
    fx.store.accept_binding(&unbind, posture(), &NoExtraChecks).unwrap();
    assert!(!covered(&fx, "aarch64"));
    assert_eq!(fx.store.binding_status(), Err(LicenceError::Unbound));
    // New steward, same Seed and key: grants continue.
    let mut r = binding_rec(3, BindState::Bound, &grant_key(), &mesh());
    r.steward_node_id = "node-new-steward".into();
    let rebind = sign_binding(&r, &op()).unwrap();
    fx.store.accept_binding(&rebind, posture(), &NoExtraChecks).unwrap();
    assert!(covered(&fx, "aarch64"));
}

#[test]
fn a_new_grant_key_voids_grants_under_the_old_one() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, DAY, &["aarch64"])).unwrap();
    let r = binding_rec(2, BindState::Bound, &sk(40), &mesh());
    fx.store.accept_binding(&sign_binding(&r, &op()).unwrap(), posture(), &NoExtraChecks).unwrap();
    assert!(!covered(&fx, "aarch64"));
    // The old key's grants are no longer accepted either.
    assert_eq!(put(&fx, &grant(2, T0, DAY, &["aarch64"])), Err(LicenceError::UntrustedKey));
}

#[test]
fn changed_mesh_nonce_orphans_the_binding_once_and_back_again() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, DAY, &["aarch64"])).unwrap();
    fx.local.set(Some(other_mesh()));
    assert!(!covered(&fx, "aarch64"));
    assert_eq!(fx.store.binding_status(), Err(LicenceError::Orphaned));
    assert_eq!(fx.store.binding_status(), Err(LicenceError::Orphaned));
    assert_eq!(fx.names(), ["binding_orphaned"]); // chained once
    fx.local.set(Some(mesh()));
    assert!(covered(&fx, "aarch64"));
}

// ── grants ───────────────────────────────────────────────────────

#[test]
fn grant_signed_by_node_unbound_key_or_for_another_mesh_is_refused() {
    let fx = Fx::new();
    fx.bind();
    for k in [sk(3), sk(4)] {
        let s = sign_grant(&grant_rec(1, T0, HOUR, &["aarch64"]), &k).unwrap();
        assert_eq!(put(&fx, &s), Err(LicenceError::UntrustedKey));
    }
    let mut rec = grant_rec(1, T0, HOUR, &["aarch64"]);
    rec.mesh_id = other_mesh().to_hex();
    let s = sign_grant(&rec, &grant_key()).unwrap();
    assert_eq!(put(&fx, &s), Err(LicenceError::WrongMesh));
    assert!(!covered(&fx, "aarch64"));
}

#[test]
fn expiry_stops_validity_but_the_record_stays() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    assert!(covered(&fx, "aarch64"));
    fx.set_now(T0 + HOUR);
    assert!(!covered(&fx, "aarch64"));
    assert!(fx.store.verified_grants().is_empty());
    // The record is kept (a tombstone for its seq): a replay is a duplicate.
    assert_eq!(put(&fx, &grant(1, T0, HOUR, &["aarch64"])), Ok(Outcome::Duplicate));
}

#[test]
fn lower_seq_is_ignored_duplicates_are_idempotent_withdrawal_stops_serving() {
    let fx = Fx::new();
    fx.bind();
    assert_eq!(put(&fx, &grant(5, T0, DAY, &["aarch64"])), Ok(Outcome::Applied));
    assert_eq!(put(&fx, &grant(5, T0, DAY, &["aarch64"])), Ok(Outcome::Duplicate));
    assert_eq!(put(&fx, &grant(4, T0, DAY, &["aarch64"])), Ok(Outcome::Ignored));
    assert!(covered(&fx, "aarch64"));
    // Withdrawal: a renewal with expires_at <= issued_at.
    assert_eq!(put(&fx, &grant(6, T0 + 10, 0, &["aarch64"])), Ok(Outcome::Applied));
    assert!(!covered(&fx, "aarch64"));
    // The earlier, still-unexpired grant cannot be replayed to undo it.
    assert_eq!(put(&fx, &grant(5, T0, DAY, &["aarch64"])), Ok(Outcome::Ignored));
    assert!(!covered(&fx, "aarch64"));
}

#[test]
fn a_newer_grant_carries_the_union_of_arches() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, DAY, &["aarch64"])).unwrap();
    // Adding an arch must keep the first.
    let dropped = put(&fx, &grant(2, T0, DAY, &["x86_64"]));
    assert_eq!(dropped, Err(LicenceError::DropsArch("aarch64".into())));
    assert_eq!(put(&fx, &grant(2, T0, DAY, &["aarch64", "x86_64"])), Ok(Outcome::Applied));
    assert!(covered(&fx, "aarch64") && covered(&fx, "x86_64"));
    // A renewal cannot swap the bytes behind an arch it already carried.
    let mut rec = grant_rec(3, T0, DAY, &["aarch64", "x86_64"]);
    rec.artifacts[0].blake3 = b3_of("armv7");
    let swapped = sign_grant(&rec, &grant_key()).unwrap();
    assert_eq!(put(&fx, &swapped), Err(LicenceError::ChangesArtifact("aarch64".into())));
}

#[test]
fn same_seq_conflict_refuses_both_and_keeps_the_earlier_grant() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, DAY, &["aarch64"])).unwrap();
    put(&fx, &grant(2, T0, DAY, &["aarch64", "x86_64"])).unwrap();
    assert!(covered(&fx, "x86_64"));
    // A different payload at seq 2 (same key).
    let rival = grant(2, T0 + 5, DAY, &["aarch64", "x86_64"]);
    assert_eq!(put(&fx, &rival), Err(LicenceError::Conflict(2)));
    assert_eq!(fx.names(), ["grant_conflict"]);
    // Both payloads at seq 2 are now refused; the seq-1 grant is what is held.
    assert!(covered(&fx, "aarch64"));
    assert!(!covered(&fx, "x86_64"));
    assert_eq!(put(&fx, &grant(2, T0, DAY, &["aarch64", "x86_64"])), Err(LicenceError::Conflict(2)));
    assert_eq!(put(&fx, &rival), Err(LicenceError::Conflict(2)));
    // A later seq is fine again.
    assert_eq!(put(&fx, &grant(3, T0, DAY, &["aarch64", "x86_64"])), Ok(Outcome::Applied));
    assert!(covered(&fx, "x86_64"));
}

#[test]
fn grant_issued_ahead_of_the_clock_is_deferred_then_accepted() {
    let fx = Fx::new();
    fx.bind();
    let g = grant(1, T0 + 600, DAY, &["aarch64"]);
    assert_eq!(put(&fx, &g), Err(LicenceError::NotYetValid));
    fx.set_now(T0 + 400); // now within the 5 min skew
    assert_eq!(put(&fx, &g), Ok(Outcome::Applied));
}

#[test]
fn grant_key_revocation_ends_every_checkout_grant() {
    let fx = Fx::new();
    let list = Arc::new(RevocationList::new(fx.dir.path().join("revoked.json")));
    fx.store.attach_revocations(list.clone());
    fx.bind();
    put(&fx, &grant(1, T0, DAY, &["aarch64"])).unwrap();
    list.revoke_subject(RevocationKind::SignerKey, &pk_hex(&grant_key()), "stolen seed").unwrap();
    assert!(!covered(&fx, "aarch64"));
    assert_eq!(put(&fx, &grant(2, T0, DAY, &["aarch64"])), Err(LicenceError::KeyRevoked));
}

#[test]
fn an_unattached_list_answers_zero_and_refuses_an_operator_notice() {
    let fx = Fx::new();
    assert_eq!(fx.store.revocation_generation(), 0);
    assert_eq!(fx.store.revoked_artifact_count(), 0);
    assert!(fx.store.revocation_list_error().is_none());
    let notice = crate::mesh_swarm_revoke::sign_revocation(
        RevocationKind::ArtifactHash,
        &"ab".repeat(32),
        "pulled",
        1,
        &op(),
    )
    .unwrap();
    let err = fx.store.apply_operator_revocation(&notice).unwrap_err();
    assert!(err.contains("not attached"), "{err}");
}

#[test]
fn an_operator_notice_revokes_the_artifact_and_a_repeat_is_a_duplicate() {
    let fx = Fx::new();
    let list = Arc::new(RevocationList::new(fx.dir.path().join("revoked-artifacts.json")));
    fx.store.attach_revocations(list);
    let hash = "ab".repeat(32);
    let notice = crate::mesh_swarm_revoke::sign_revocation(
        RevocationKind::ArtifactHash,
        &hash,
        "pulled",
        1,
        &op(),
    )
    .unwrap();
    assert_eq!(fx.store.apply_operator_revocation(&notice), Ok(true));
    assert!(fx.store.is_hash_revoked(&hash));
    assert_eq!(fx.store.revoked_artifact_count(), 1);
    assert!(fx.store.revocation_generation() >= 1);
    assert_eq!(fx.store.apply_operator_revocation(&notice), Ok(false));
}

// ── clock floor ──────────────────────────────────────────────────

#[test]
fn a_clock_set_back_cannot_revive_a_grant() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    fx.set_now(T0 + 2 * HOUR);
    assert!(!covered(&fx, "aarch64"));
    fx.set_now(T0 + 10); // back inside the validity window
    assert!(!covered(&fx, "aarch64"));
    assert!(fx.store.floor().unwrap() >= T0 + 2 * HOUR);
}

#[test]
fn the_floor_and_the_store_survive_a_restart() {
    let mut fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    fx.set_now(T0 + 2 * HOUR);
    fx.store.tick(); // records and persists the high-water mark
    fx.set_now(T0 + 10);
    fx.restart();
    assert!(fx.store.active_binding().is_some());
    assert!(!covered(&fx, "aarch64"), "the persisted floor outlives the process");
    // And the grant itself was persisted: a later grant sees seq 1 as held.
    fx.set_now(T0 + 3 * HOUR);
    assert_eq!(put(&fx, &grant(1, T0, HOUR, &["aarch64"])), Ok(Outcome::Duplicate));
}

#[test]
fn a_forward_jump_is_capped_never_lowered_and_only_reset_undoes_it() {
    let mut fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    fx.set_now(T0 + 90 * DAY); // the clock briefly ran 90 days ahead
    fx.store.tick();
    fx.set_now(T0 + 60); // and was corrected
    fx.restart();
    // The mark is capped at issued + 30 days but is not lowered: the expired
    // grant stays dead after the clock is set back.
    assert!(!covered(&fx, "aarch64"));
    assert_eq!(fx.store.floor(), Some(T0 + 30 * DAY));
    fx.store.reset_floor().unwrap(); // Admin, chained
    assert_eq!(fx.store.floor(), Some(T0 + 60));
    assert!(covered(&fx, "aarch64"));
    assert_eq!(fx.names(), ["floor_reset"]);
}

#[test]
fn a_genuine_long_gap_then_a_set_back_clock_does_not_revive_a_grant() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    fx.set_now(T0 + 35 * DAY);
    fx.store.tick();
    fx.set_now(T0 + 10); // back inside the old grant's window
    assert!(!covered(&fx, "aarch64"));
    assert!(fx.names().is_empty());
}

#[test]
fn no_high_water_mark_is_recorded_before_the_first_grant() {
    let fx = Fx::new();
    fx.bind();
    fx.set_now(T0 + 90 * DAY);
    fx.store.tick();
    assert_eq!(fx.store.floor(), Some(0));
}

#[test]
fn reset_floor_restarts_the_high_water_mark_and_chains() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    fx.set_now(T0 + 5 * DAY);
    fx.store.tick();
    fx.set_now(T0 + 100);
    fx.store.reset_floor().unwrap();
    assert_eq!(fx.store.floor(), Some(T0 + 100));
    assert!(covered(&fx, "aarch64"));
    assert_eq!(fx.names(), ["floor_reset"]);
}

#[test]
fn floor_preview_lists_what_a_reset_would_revive_and_changes_nothing() {
    let fx = Fx::new();
    assert!(fx.store.floor_preview().is_err(), "needs a binding in effect");
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    let p = fx.store.floor_preview().unwrap();
    assert!(p.revived.is_empty(), "nothing is held expired by the floor yet");
    fx.set_now(T0 + 5 * DAY);
    fx.store.tick();
    fx.set_now(T0 + 100);
    let p = fx.store.floor_preview().unwrap();
    assert_eq!((p.now, p.floor, p.floor_after), (T0 + 100, T0 + 5 * DAY, T0 + 100));
    assert_eq!(p.revived.len(), 1);
    assert_eq!(p.revived[0].cog_id, grant_rec(1, T0, HOUR, &["aarch64"]).cog_id);
    // Looking changed nothing.
    assert!(!covered(&fx, "aarch64"));
    assert!(fx.names().is_empty());
    fx.store.reset_floor().unwrap();
    assert!(covered(&fx, "aarch64"));
}

#[test]
fn reset_floor_checked_refuses_when_the_floor_moved_since_the_preview() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, HOUR, &["aarch64"])).unwrap();
    fx.set_now(T0 + 5 * DAY);
    fx.store.tick();
    fx.set_now(T0 + 100);
    let shown = fx.store.floor_preview().unwrap();
    assert_eq!(
        fx.store.reset_floor_checked(Some(shown.floor + 1)),
        Err(LicenceError::CheckFailed("floor_changed".into()))
    );
    assert!(!covered(&fx, "aarch64"), "a refused reset changed nothing");
    let done = fx.store.reset_floor_checked(Some(shown.floor)).unwrap();
    assert_eq!(done, shown);
    assert!(covered(&fx, "aarch64"));
    assert_eq!(fx.names(), ["floor_reset"]);
}

// ── persistence, fail closed ─────────────────────────────────────

#[test]
fn files_are_private_and_written_atomically() {
    let fx = Fx::new();
    fx.bind();
    fx.approvals.accept(&approval(&[sha_of("aarch64")])).unwrap();
    for f in ["checkout_grants.json", "checkout_approvals.json"] {
        let p = fx.dir.path().join(f);
        assert!(p.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
    let stray: Vec<_> = std::fs::read_dir(fx.dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(stray.is_empty(), "no temp file left behind");
}

fn open_grants(dir: &std::path::Path) -> Result<CheckoutGrantStore, LicenceError> {
    CheckoutGrantStore::open(dir, anchors(), LocalMeshId::new(mesh()), system_clock())
}

#[test]
fn a_malformed_file_fails_closed_and_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checkout_grants.json");
    std::fs::write(&path, b"not json{{{").unwrap();
    assert!(matches!(open_grants(dir.path()), Err(LicenceError::Malformed(_))));
    let s = CheckoutGrantStore::open_or_poisoned(
        dir.path(),
        anchors(),
        LocalMeshId::new(mesh()),
        system_clock(),
    );
    assert!(s.poisoned().is_some());
    let r = s.accept_binding(&binding(1, BindState::Bound), posture(), &NoExtraChecks);
    assert!(matches!(r, Err(LicenceError::Poisoned(_))));
    assert!(s.active_binding().is_none());
    assert_eq!(std::fs::read(&path).unwrap(), b"not json{{{");
}

#[test]
fn a_tampered_file_fails_the_load_time_reverification() {
    let fx = Fx::new();
    fx.bind();
    put(&fx, &grant(1, T0, DAY, &["aarch64"])).unwrap();
    let path = fx.dir.path().join("checkout_grants.json");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("fall-detect", "fall-detecT")).unwrap();
    assert!(open_grants(fx.dir.path()).is_err());
}

#[test]
fn an_oversize_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let big = vec![b' '; (MAX_STORE_BYTES + 1) as usize];
    std::fs::write(dir.path().join("checkout_grants.json"), &big).unwrap();
    assert!(matches!(open_grants(dir.path()), Err(LicenceError::TooLarge)));
    std::fs::write(dir.path().join("checkout_approvals.json"), &big).unwrap();
    let r = ApprovalStore::open(dir.path(), anchors(), LocalMeshId::new(mesh()));
    assert!(matches!(r, Err(LicenceError::TooLarge)));
}

#[test]
fn no_local_mesh_id_accepts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let s = CheckoutGrantStore::open(dir.path(), anchors(), LocalMeshId::unset(), system_clock())
        .unwrap();
    let r = s.accept_binding(&binding(1, BindState::Bound), posture(), &NoExtraChecks);
    assert_eq!(r, Err(LicenceError::NoLocalMesh));
}

// ── approvals ────────────────────────────────────────────────────

#[test]
fn duplicate_approval_is_idempotent_and_approvals_are_additive() {
    let fx = Fx::new();
    let (a, b) = (sha_of("aarch64"), sha_of("x86_64"));
    assert_eq!(
        fx.approvals.accept(&approval(std::slice::from_ref(&a))),
        Ok(Outcome::Applied)
    );
    assert_eq!(
        fx.approvals.accept(&approval(std::slice::from_ref(&a))),
        Ok(Outcome::Duplicate)
    );
    // Re-signed later (different approved_at): same content key, still a duplicate.
    let mut later = approval_rec(std::slice::from_ref(&a), &mesh());
    later.approved_at += 500;
    assert_eq!(
        fx.approvals.accept(&sign_approval(&later, &op()).unwrap()),
        Ok(Outcome::Duplicate)
    );
    // A second approval adds; it does not replace the first.
    assert_eq!(
        fx.approvals.accept(&approval(std::slice::from_ref(&b))),
        Ok(Outcome::Applied)
    );
    assert_eq!(fx.approvals.len(), 2);
    assert!(fx.approvals.covering("fall-detect", "1.2.0", &a).is_some());
    assert!(fx.approvals.covering("fall-detect", "1.2.0", &b).is_some());
    assert!(fx.approvals.covering("fall-detect", "1.2.1", &a).is_none());
}

#[test]
fn approval_for_another_mesh_or_signer_is_refused() {
    let fx = Fx::new();
    let shas = [sha_of("aarch64")];
    let wrong = sign_approval(&approval_rec(&shas, &other_mesh()), &op()).unwrap();
    assert_eq!(fx.approvals.accept(&wrong), Err(LicenceError::WrongMesh));
    let rogue = sign_approval(&approval_rec(&shas, &mesh()), &grant_key()).unwrap();
    assert_eq!(fx.approvals.accept(&rogue), Err(LicenceError::UntrustedKey));
    assert!(fx.approvals.is_empty());
}

#[test]
fn approvals_persist_and_a_mesh_change_orphans_them() {
    let mut fx = Fx::new();
    let a = sha_of("aarch64");
    fx.approvals
        .accept(&approval(std::slice::from_ref(&a)))
        .unwrap();
    fx.restart();
    assert!(fx.approvals.covering("fall-detect", "1.2.0", &a).is_some());
    fx.local.set(Some(other_mesh()));
    assert!(fx.approvals.covering("fall-detect", "1.2.0", &a).is_none());
    assert_eq!(fx.approvals.orphaned().len(), 1);
    fx.local.set(Some(mesh()));
    assert!(fx.approvals.orphaned().is_empty());
}
