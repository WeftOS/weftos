//! Regression tests for the Vault V1 crypto review (reports/review-vault-v1-crypto-20261115.md), one or
//! more per finding. All values are fake; Argon2id runs the TEST profile unless a test says otherwise.

use crate::VaultError;
use crate::keybag::{KIND_RECOVERY, Keybag, Pins, WrapperBody};
use crate::passcode_kdf::KdfParams;
use crate::tests::{PASS, T, new_vault, pass_id};
use crate::vault::{NewVault, Vault};

/// H1, probe R1: a device compromised before a rotation holds the old VK and the signing seed. It
/// re-signs a pre-rotation keybag with a higher sequence. A device that has seen the rotation must refuse
/// it, even though the signer matches and the sequence is higher.
#[test]
fn h1_pinned_device_refuses_a_pre_rotation_keybag_resigned_with_a_higher_sequence() {
    let mut vault = new_vault(true).vault;
    let old_kb = vault.keybag().clone();
    let stolen = vault.signing.clone();
    let mut pins = Pins::default();
    Vault::unlock(vault.keybag_bytes(), PASS, &mut pins).unwrap();
    vault
        .rotate_vk_unchecked(PASS, None, &mut Pins::default())
        .unwrap();
    // This device learns of the rotation by unlocking the new keybag (seq 2, generation 2).
    Vault::unlock(vault.keybag_bytes(), PASS, &mut pins).unwrap();
    assert_eq!(pins.vk_generation, 2);

    let mut forged = old_kb.clone();
    forged.sequence = 99;
    let bytes = forged.sign(&stolen);
    assert_eq!(
        Vault::unlock(&bytes, PASS, &mut pins).unwrap_err(),
        VaultError::GenerationRollback { got: 1, seen: 2 }
    );
    assert_eq!(
        Keybag::parse(&bytes, &pins).unwrap_err(),
        VaultError::GenerationRollback { got: 1, seen: 2 }
    );
    // The refusal leaves the pins where they were.
    assert_eq!(pins.highest.unwrap().sequence, 2);
}

/// H1: the creating device is pinned from the start (signer, sequence 1, generation 1).
#[test]
fn h1_new_vault_returns_pins_for_the_creating_device() {
    let NewVault { vault, pins, .. } = new_vault(true);
    assert_eq!(pins.sign_pub, Some(vault.keybag().sign_pub));
    assert_eq!(pins.highest.unwrap().sequence, 1);
    assert_eq!(pins.vk_generation, 1);
    let other = new_vault(true).vault;
    let mut p = pins.clone();
    assert_eq!(
        Vault::unlock(other.keybag_bytes(), PASS, &mut p).unwrap_err(),
        VaultError::SignerMismatch
    );
}

/// M1, probe R2: the store replays the keybag from before a rotation to the device that did the rotation.
#[test]
fn m1_replay_of_the_keybag_from_before_a_rotation_is_refused() {
    let NewVault {
        mut vault,
        mut pins,
        ..
    } = new_vault(true);
    let before = vault.keybag_bytes().to_vec();
    vault.rotate_vk_unchecked(PASS, None, &mut pins).unwrap();
    assert_eq!(
        Vault::unlock(&before, PASS, &mut pins).unwrap_err(),
        VaultError::GenerationRollback { got: 1, seen: 2 }
    );
    // Without the generation pin the sequence pin alone refuses it too.
    let mut seq_only = pins.clone();
    seq_only.vk_generation = 0;
    assert_eq!(
        Vault::unlock(&before, PASS, &mut seq_only).unwrap_err(),
        VaultError::Rollback { got: 1, seen: 2 }
    );
    assert!(Vault::unlock(vault.keybag_bytes(), PASS, &mut pins).is_ok());
}

/// M1: the same replay after `remove_wrapper` of a (compromised) recovery wrapper.
#[test]
fn m1_replay_of_the_keybag_from_before_remove_wrapper_is_refused() {
    let NewVault {
        mut vault,
        recovery_key,
        mut pins,
    } = new_vault(true);
    let before = vault.keybag_bytes().to_vec();
    let rec = vault
        .keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == KIND_RECOVERY)
        .unwrap()
        .id;
    vault.remove_wrapper(&rec, &mut pins).unwrap();
    assert_eq!(
        Vault::unlock_with_recovery(&before, &recovery_key, &mut pins).unwrap_err(),
        VaultError::Rollback { got: 1, seen: 2 }
    );
}

/// M1: the same replay after `change_passcode` (the old passcode must stop working on this device).
#[test]
fn m1_replay_of_the_keybag_from_before_change_passcode_is_refused() {
    let NewVault {
        mut vault,
        mut pins,
        ..
    } = new_vault(true);
    let before = vault.keybag_bytes().to_vec();
    let id = pass_id(&vault);
    vault
        .change_passcode_unchecked(&id, "FAKE-new-passcode-0009", T, &mut pins)
        .unwrap();
    assert_eq!(
        Vault::unlock(&before, PASS, &mut pins).unwrap_err(),
        VaultError::Rollback { got: 1, seen: 2 }
    );
}

/// M1: a write never runs against pins of another vault or pins that are ahead of this vault, and a
/// failed write leaves the pins untouched.
#[test]
fn m1_writes_check_the_pins_and_a_failed_write_does_not_advance_them() {
    let NewVault {
        mut vault,
        mut pins,
        ..
    } = new_vault(true);
    let mut stale = Vault::unlock(vault.keybag_bytes(), PASS, &mut Pins::default()).unwrap();
    vault.add_recovery_wrapper(&mut pins).unwrap();
    // `stale` still holds sequence 1; the device has seen sequence 2.
    let snapshot = pins.clone();
    assert_eq!(
        stale.add_recovery_wrapper(&mut pins).unwrap_err(),
        VaultError::Rollback { got: 2, seen: 2 }
    );
    assert_eq!(pins.highest, snapshot.highest);

    let mut foreign = new_vault(true).pins;
    assert_eq!(
        vault.add_recovery_wrapper(&mut foreign).unwrap_err(),
        VaultError::SignerMismatch
    );

    // A refused change (removing the signing seed wrapper) does not touch the pins.
    let seed = vault
        .keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == crate::keybag::KIND_SIGNSEED)
        .unwrap()
        .id;
    assert!(vault.remove_wrapper(&seed, &mut pins).is_err());
    assert_eq!(pins.highest, snapshot.highest);
}

/// M2: rotation must not carry below-policy KDF parameters forward silently. It refuses, the vault and
/// the pins stay as they were, `below_policy_wrappers` names the wrapper, and after a passcode change at
/// policy the rotation goes through.
#[test]
fn m2_rotation_refuses_a_below_policy_passcode_wrapper() {
    let NewVault {
        mut vault,
        mut pins,
        ..
    } = new_vault(true);
    let before = vault.keybag_bytes().to_vec();
    let id = pass_id(&vault);
    assert_eq!(vault.below_policy_wrappers(), vec![id]);
    assert_eq!(
        vault.rotate_vk(PASS, None, &mut pins).unwrap_err(),
        VaultError::KdfPolicy
    );
    assert_eq!(vault.keybag_bytes(), &before[..]);
    assert_eq!(pins.highest.unwrap().sequence, 1);

    // The cheapest parameters the policy allows (19 MiB, t=2) keep this test fast.
    let floor = KdfParams {
        m_kib: 19456,
        t: 2,
        p: 1,
    };
    floor.check_new_policy().unwrap();
    vault
        .change_passcode(&id, "FAKE-policy-passcode-0010", floor, &mut pins)
        .unwrap();
    assert!(vault.below_policy_wrappers().is_empty());
    let r = vault
        .rotate_vk("FAKE-policy-passcode-0010", None, &mut pins)
        .unwrap();
    assert_eq!((r.new_generation, r.passcode_resealed), (2, 1));
    match &vault.keybag().wrapper(&id).unwrap().body {
        WrapperBody::Passcode { params, .. } => assert_eq!(*params, floor),
        other => panic!("not a passcode wrapper: {other:?}"),
    }
    assert_eq!((pins.highest.unwrap().sequence, pins.vk_generation), (3, 2));
}

/// L1, probe R4: a keybag at `u64::MAX` must give an error on the next write, never a wrap or a panic.
#[test]
fn l1_sequence_exhaustion_is_an_error() {
    let mut vault = new_vault(true).vault;
    vault.keybag.sequence = u64::MAX;
    vault.bytes = vault.keybag.sign(&vault.signing);
    let before = vault.keybag_bytes().to_vec();
    let mut pins = Pins::default();
    assert_eq!(
        vault.add_recovery_wrapper(&mut pins).unwrap_err(),
        VaultError::SequenceExhausted
    );
    assert_eq!(vault.keybag_bytes(), &before[..]);
    assert!(pins.highest.is_none());
}
