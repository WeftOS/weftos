//! `Vault::resolve_write` (review F2): the consuming resolver for a write the store accepted. All values
//! are fake.

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
