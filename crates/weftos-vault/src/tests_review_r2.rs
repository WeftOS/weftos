//! Regression tests for the round-2 crypto review (reports/review-vault-v1-crypto-r2-20261115.md), one
//! or more per finding. All values are fake; Argon2id runs the TEST profile.

use crate::VaultError;
use crate::keybag::Pins;
use crate::pins::PINS_LEN;
use crate::tests::new_vault;

/// R2-L1: pins are read through getters and persisted as bytes; loading is strict.
#[test]
fn r2_l1_pins_round_trip_as_bytes_and_loading_is_strict() {
    let nv = new_vault(true);
    let b = nv.pins.to_bytes();
    assert_eq!(b.len(), PINS_LEN);
    assert_eq!(Pins::from_bytes(&b).unwrap(), nv.pins);
    assert_eq!(nv.pins.sign_pub(), Some(nv.vault.keybag().sign_pub));
    assert_eq!(nv.pins.vk_generation(), 1);
    let empty = Pins::default().to_bytes();
    assert_eq!(Pins::from_bytes(&empty).unwrap(), Pins::default());

    let bad = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut v = b.to_vec();
        f(&mut v);
        Pins::from_bytes(&v).unwrap_err()
    };
    let malformed = VaultError::Malformed("pins");
    assert_eq!(bad(&|v| v.truncate(PINS_LEN - 1)), malformed);
    assert_eq!(bad(&|v| v.push(0)), malformed);
    assert_eq!(bad(&|v| v[0] ^= 1), malformed);
    assert_eq!(bad(&|v| v[4] = 2), malformed);
    assert_eq!(bad(&|v| v[5] = 2), malformed);
    assert_eq!(bad(&|v| v[38..46].fill(0)), malformed);
    assert_eq!(bad(&|v| v[78..82].fill(0)), malformed);
    let mut e = empty.to_vec();
    e[10] = 1;
    assert_eq!(Pins::from_bytes(&e).unwrap_err(), malformed);
}
