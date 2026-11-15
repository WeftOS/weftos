//! KDF floor change (owner decision 2026-11-15, docs/research/vault-v1-kdf-floor-2026-11-15.md): new and
//! rewritten passcode wrappers need at least 64 MiB and t=3; stored wrappers below that still unlock and
//! are reported. All values are fake. Only `create_accepts_the_floor` (and the M2 regression test) run
//! Argon2id at full cost; the refusals happen before any KDF work.

use crate::VaultError;
use crate::keybag::{Pins, WrapperBody};
use crate::passcode_kdf::KdfParams;
use crate::tests::{PASS, T, new_vault, pass_id};
use crate::vault::{NewVault, Vault};

const fn kp(m_kib: u32, t: u32, p: u32) -> KdfParams {
    KdfParams { m_kib, t, p }
}

/// 64 MiB at t=2: enough memory, too few passes.
const M64_T2: KdfParams = kp(65536, 2, 1);
/// 32 MiB at t=3: enough passes, too little memory.
const M32_T3: KdfParams = kp(32768, 3, 1);
/// The spec 3.1 floor before the change; a realistic legacy wrapper.
const OLD_FLOOR: KdfParams = kp(19456, 2, 1);

#[test]
fn floor_is_64_mib_and_t3_and_recommended_sits_on_it() {
    assert_eq!((KdfParams::MIN_NEW_M_KIB, KdfParams::MIN_NEW_T), (65536, 3));
    assert_eq!(KdfParams::RECOMMENDED, kp(65536, 3, 1));
    for below in [
        M64_T2,
        M32_T3,
        kp(65535, 3, 1),
        OLD_FLOOR,
        kp(46080, 1, 1),
        T,
    ] {
        assert_eq!(
            below.check_new_policy(),
            Err(VaultError::KdfPolicy),
            "{below:?}"
        );
        // Still format-valid: a stored wrapper with these parameters parses and unlocks.
        assert!(below.check_format().is_ok(), "{below:?}");
    }
    for ok in [
        kp(65536, 3, 1),
        kp(65536, 3, 4),
        kp(131072, 3, 1),
        kp(65536, 4, 1),
    ] {
        assert!(ok.check_new_policy().is_ok(), "{ok:?}");
    }
}

#[test]
fn create_refuses_below_the_floor() {
    for below in [M64_T2, M32_T3, OLD_FLOOR] {
        assert_eq!(
            Vault::create(PASS, below).unwrap_err(),
            VaultError::KdfPolicy,
            "{below:?}"
        );
    }
}

#[test]
fn change_passcode_refuses_below_the_floor_and_changes_nothing() {
    let NewVault {
        mut vault,
        mut pins,
        ..
    } = new_vault();
    let id = pass_id(&vault);
    let before = (vault.keybag_bytes().to_vec(), pins.clone());
    for below in [M64_T2, M32_T3, OLD_FLOOR] {
        assert_eq!(
            vault
                .write_now(&mut pins, |v, p| {
                    v.change_passcode(&id, "FAKE-floor-passcode-0001", below, p)
                })
                .unwrap_err(),
            VaultError::KdfPolicy,
            "{below:?}"
        );
    }
    assert_eq!((vault.keybag_bytes().to_vec(), pins), before);
}

/// The real floor: one full-cost Argon2id run. (`m2_rotation_refuses_a_below_policy_passcode_wrapper`
/// covers `change_passcode` and `rotate_vk` at the floor.)
#[test]
fn create_accepts_the_floor() {
    let nv = Vault::create(PASS, KdfParams::RECOMMENDED).unwrap();
    assert!(nv.vault.below_policy_wrappers().is_empty());
    match &nv.vault.keybag().wrapper(&pass_id(&nv.vault)).unwrap().body {
        WrapperBody::Passcode { params, .. } => assert_eq!(*params, KdfParams::RECOMMENDED),
        other => panic!("not a passcode wrapper: {other:?}"),
    }
}

/// A wrapper written under the old floor (19 MiB, t=2) still unlocks and is reported; rotation refuses
/// to re-seal it; writes that carry it unchanged go through; a passcode change below the floor is refused.
#[test]
fn legacy_below_floor_wrapper_unlocks_is_reported_and_blocks_rotation() {
    let created = Vault::create_unchecked(PASS, OLD_FLOOR).unwrap();
    let stored = created.vault.keybag_bytes().to_vec();
    let id = pass_id(&created.vault);

    let mut pins = Pins::default();
    let mut vault = Vault::unlock(&stored, PASS, &mut pins).unwrap();
    assert_eq!(vault.below_policy_wrappers(), vec![id]);

    let before = (vault.keybag_bytes().to_vec(), pins.clone());
    assert_eq!(
        vault
            .write_now(&mut pins, |v, p| v.rotate_vk(PASS, None, p))
            .unwrap_err(),
        VaultError::KdfPolicy
    );
    assert_eq!((vault.keybag_bytes().to_vec(), pins.clone()), before);

    let legacy_body = vault.keybag().wrapper(&id).unwrap().body.clone();
    vault
        .write_now(&mut pins, |v, p| v.add_recovery_wrapper(p))
        .unwrap();
    assert_eq!(vault.keybag().wrapper(&id).unwrap().body, legacy_body);
    assert_eq!(vault.below_policy_wrappers(), vec![id]);

    for below in [M64_T2, M32_T3, OLD_FLOOR] {
        assert_eq!(
            vault
                .write_now(&mut pins, |v, p| {
                    v.change_passcode(&id, "FAKE-floor-passcode-0002", below, p)
                })
                .unwrap_err(),
            VaultError::KdfPolicy
        );
    }
    // The keybag the device now holds still unlocks by the legacy passcode.
    let again = vault.keybag_bytes().to_vec();
    assert!(Vault::unlock(&again, PASS, &mut pins).is_ok());
}
