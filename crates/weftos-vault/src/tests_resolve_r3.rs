//! Round-3 review probes for `resolve_write` (R3-1, R3-2): tamper evidence and foreign keybags come back
//! as `Refused` with the pending write intact; transient reads stay `Retry`. All values are fake.

use crate::error::VaultError;
use crate::keybag::Pins;
use crate::tests::{PASS, new_vault};
use crate::vault::Vault;
use crate::write::Resolution;

struct Store(Vec<u8>);
impl Store {
    fn put_if_match(&mut self, base: &[u8], new: &[u8]) -> bool {
        let ok = self.0 == base;
        if ok {
            self.0 = new.to_vec();
        }
        ok
    }
}

/// A foreign-signed read-back under pinned pins is `Refused(SignerMismatch)`, every time, pins and vault
/// untouched, and the real read-back still commits with the recovery key intact.
#[test]
fn foreign_signer_is_refused_with_cause() {
    let nv = new_vault();
    let other = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins).unwrap();
    let pins_before = pins.clone();
    let pa = a.add_recovery_wrapper(&pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    let mut pa = pa;
    for _ in 0..3 {
        pa = match a
            .resolve_write(pa, other.vault.keybag_bytes(), &mut pins)
            .unwrap()
        {
            Resolution::Refused { pending, cause } => {
                assert!(matches!(cause, VaultError::SignerMismatch), "{cause:?}");
                *pending
            }
            other => panic!("{other:?}"),
        };
    }
    assert_eq!(pins, pins_before);
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins).unwrap(),
        Resolution::Committed(_)
    ));
}

/// A fork at the pinned sequence (signed by the vault key, other bytes) is `Refused(Fork)`.
#[test]
fn fork_at_base_is_refused_with_cause() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins).unwrap();
    let mut forged = a.keybag.clone();
    forged.updated_unix += 1;
    let forged = forged.sign(&a.signing);
    let pa = a.add_recovery_wrapper(&pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    match a.resolve_write(pa, &forged, &mut pins).unwrap() {
        Resolution::Refused { pending, cause } => {
            assert!(matches!(cause, VaultError::Fork { .. }), "{cause:?}");
            assert!(matches!(
                a.resolve_write(*pending, &store.0, &mut pins).unwrap(),
                Resolution::Committed(_)
            ));
        }
        other => panic!("{other:?}"),
    }
}

/// With unpinned pins, a keybag of another vault does not consume the write: `Refused(StoreMismatch)`,
/// nothing pinned, and the pending write still commits against the real read-back.
#[test]
fn unpinned_foreign_vault_is_refused_not_consumed() {
    let nv = new_vault();
    let other = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins).unwrap();
    let pa = a.add_recovery_wrapper(&pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    let mut fresh = Pins::default();
    let pa = match a
        .resolve_write(pa, other.vault.keybag_bytes(), &mut fresh)
        .unwrap()
    {
        Resolution::Refused { pending, cause } => {
            assert!(matches!(cause, VaultError::StoreMismatch), "{cause:?}");
            *pending
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(fresh, Pins::default(), "nothing pinned for a foreign vault");
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins).unwrap(),
        Resolution::Committed(_)
    ));
}

/// A lagging pre-rotation replica stays `Retry`: the benign case.
#[test]
fn pre_rotation_lag_stays_retry() {
    let nv = new_vault();
    let base = nv.vault.keybag_bytes().to_vec();
    let mut store = Store(base.clone());
    let mut pins = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins).unwrap();
    let rot = a.rotate_vk_unchecked(PASS, None, &pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), rot.keybag_bytes()));
    a.commit_write(rot, &store.0, &mut pins).unwrap();
    let pa = a.add_recovery_wrapper(&pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    assert!(matches!(
        a.resolve_write(pa, &base, &mut pins).unwrap(),
        Resolution::Retry(_)
    ));
}

/// After `NotLanded` and `Landed`, further writes from the stale vault are refused with `Rollback`
/// until it re-unlocks (R3-4); the next write then commits.
#[test]
fn stale_vault_refused_until_reunlock() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins_a = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let pa = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins_a).unwrap(),
        Resolution::Landed(_)
    ));
    assert!(matches!(
        a.add_recovery_wrapper(&pins_a),
        Err(VaultError::Rollback { .. })
    ));
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();
    let pa = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(!store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins_a).unwrap(),
        Resolution::NotLanded
    ));
    assert!(matches!(
        a.add_recovery_wrapper(&pins_a),
        Err(VaultError::Rollback { .. })
    ));
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let pa = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins_a).unwrap(),
        Resolution::Committed(_)
    ));
}
