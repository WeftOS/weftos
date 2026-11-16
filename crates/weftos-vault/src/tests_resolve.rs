//! `Vault::resolve_write` (review F2): the consuming resolver for a write the store accepted. All values
//! are fake.

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

/// The write landed, another device wrote on top before the read-back: `resolve_write` hands the recovery
/// key back, installs nothing, and the key is the one whose wrapper the store holds.
#[test]
fn resolve_hands_back_the_recovery_key_when_a_racing_writer_buried_the_write() {
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

    let before = a.keybag_bytes().to_vec();
    match a.resolve_write(pa, &store.0, &mut pins_a).unwrap() {
        Resolution::Landed(key) => {
            // The returned key is the one the store's wrapper was sealed under: recovery unlock works.
            let code = key.to_code();
            let mut p = Pins::default();
            let rk = crate::RecoveryKey::from_code(&code).unwrap();
            Vault::unlock_with_recovery(&store.0, &rk, &mut p).unwrap();
        }
        other => panic!("expected Landed, got {other:?}"),
    }
    assert_eq!(a.keybag_bytes(), before.as_slice());
}

/// The exact read-back commits, as `commit_write` does.
#[test]
fn resolve_commits_the_exact_read_back() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins).unwrap();
    let pa = a.add_recovery_wrapper(&pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins).unwrap(),
        Resolution::Committed(_)
    ));
    assert_eq!(a.keybag_bytes(), store.0.as_slice());
}

/// A refused write: the store holds another device's keybag, so the result is not handed back
/// (`NotLanded` carries nothing; the pending write, with its recovery key, is consumed and dropped).
#[test]
fn resolve_returns_nothing_for_a_refused_write() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins_a = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();
    let pa = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(!store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins_a).unwrap(),
        Resolution::NotLanded
    ));
}

/// R2-1: after `Landed` for a rotation the pins hold the verified read-back, so a store that serves the
/// pre-rotation keybag later is refused.
#[test]
fn landed_rotation_records_the_read_back_in_the_pins() {
    let nv = new_vault();
    let base = nv.vault.keybag_bytes().to_vec();
    let mut store = Store(base.clone());
    let mut pins_a = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let rot = a.rotate_vk_unchecked(PASS, None, &pins_a).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), rot.keybag_bytes()));
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();

    assert!(matches!(
        a.resolve_write(rot, &store.0, &mut pins_a).unwrap(),
        Resolution::Landed(_)
    ));
    assert_eq!(pins_a.vk_generation(), 2);
    assert_eq!(pins_a.highest().unwrap().sequence, 3);
    let mut p = pins_a.clone();
    assert!(matches!(
        Vault::unlock(&base, PASS, &mut p),
        Err(VaultError::GenerationRollback { .. } | VaultError::Rollback { .. })
    ));
    // The read-back itself still unlocks.
    Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
}

/// R2-1: a verified newer keybag that lacks the write is `NotLanded`, and the pins still record it.
#[test]
fn not_landed_records_the_verified_newer_keybag() {
    let nv = new_vault();
    let base = nv.vault.keybag_bytes().to_vec();
    let mut store = Store(base.clone());
    let mut pins_a = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();
    let pa = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins_a).unwrap(),
        Resolution::NotLanded
    ));
    assert_eq!(pins_a.highest().unwrap().sequence, 2);
    let mut p = pins_a.clone();
    assert!(Vault::unlock(&base, PASS, &mut p).is_err());
}

/// R2-2: a stale read-after-write (the base keybag) hands the pending write back; a second GET commits
/// and the recovery key is still there to show.
#[test]
fn stale_read_back_is_retried_not_consumed() {
    let nv = new_vault();
    let base = nv.vault.keybag_bytes().to_vec();
    let mut store = Store(base.clone());
    let mut pins = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins).unwrap();
    let pa = a.add_recovery_wrapper(&pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    let pa = match a.resolve_write(pa, &base, &mut pins).unwrap() {
        Resolution::Retry(p) => *p,
        other => panic!("expected Retry, got {other:?}"),
    };
    assert_eq!(
        pins.highest().unwrap().sequence,
        1,
        "a stale read moves nothing"
    );
    match a.resolve_write(pa, &store.0, &mut pins).unwrap() {
        Resolution::Committed(key) => {
            let rk = crate::RecoveryKey::from_code(&key.to_code()).unwrap();
            Vault::unlock_with_recovery(&store.0, &rk, &mut Pins::default()).unwrap();
        }
        other => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(a.keybag_bytes(), store.0.as_slice());
}

/// R2-2: a truncated read-back hands the pending write back too, and so does garbage.
#[test]
fn unparseable_read_back_is_retried_not_consumed() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins).unwrap();
    let pa = a.add_recovery_wrapper(&pins).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    let cut = store.0[..store.0.len() - 1].to_vec();
    let pa = match a.resolve_write(pa, &cut, &mut pins).unwrap() {
        Resolution::Retry(p) => *p,
        other => panic!("expected Retry, got {other:?}"),
    };
    let pa = match a.resolve_write(pa, b"garbage", &mut pins).unwrap() {
        Resolution::Retry(p) => *p,
        other => panic!("expected Retry, got {other:?}"),
    };
    assert!(matches!(
        a.resolve_write(pa, &store.0, &mut pins).unwrap(),
        Resolution::Committed(_)
    ));
}
