//! Regression tests for the Vault V1 crypto review (reports/review-vault-v1-crypto-20261115.md), one or
//! more per finding. All values are fake; Argon2id runs the TEST profile unless a test says otherwise.

use crate::VaultError;
use crate::index::IndexEntry;
use crate::keybag::{
    KIND_PASSCODE, KIND_RECOVERY, Keybag, MAX_PASSCODE_WRAPPERS, Pins, WrapperBody,
};
use crate::passcode_kdf::KdfParams;
use crate::tests::{PASS, T, new_vault, pass_id};
use crate::vault::{NewVault, Vault};

/// H1, probe R1: a device compromised before a rotation holds the old VK and the signing seed. It
/// re-signs a pre-rotation keybag with a higher sequence. A device that has seen the rotation must refuse
/// it, even though the signer matches and the sequence is higher.
#[test]
fn h1_pinned_device_refuses_a_pre_rotation_keybag_resigned_with_a_higher_sequence() {
    let mut vault = new_vault().vault;
    let old_kb = vault.keybag().clone();
    let stolen = vault.signing.clone();
    let mut pins = Pins::default();
    Vault::unlock(vault.keybag_bytes(), PASS, &mut pins).unwrap();
    vault
        .write_now(&mut Pins::default(), |v, p| {
            v.rotate_vk_unchecked(PASS, None, p)
        })
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
    let NewVault { vault, pins, .. } = new_vault();
    assert_eq!(pins.sign_pub, Some(vault.keybag().sign_pub));
    assert_eq!(pins.highest.unwrap().sequence, 1);
    assert_eq!(pins.vk_generation, 1);
    let other = new_vault().vault;
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
    } = new_vault();
    let before = vault.keybag_bytes().to_vec();
    vault
        .write_now(&mut pins, |v, p| v.rotate_vk_unchecked(PASS, None, p))
        .unwrap();
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
    } = new_vault();
    let before = vault.keybag_bytes().to_vec();
    let rec = vault
        .keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == KIND_RECOVERY)
        .unwrap()
        .id;
    vault
        .write_now(&mut pins, |v, p| v.remove_wrapper(&rec, p))
        .unwrap();
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
    } = new_vault();
    let before = vault.keybag_bytes().to_vec();
    let id = pass_id(&vault);
    vault
        .write_now(&mut pins, |v, p| {
            v.change_passcode_unchecked(&id, "FAKE-new-passcode-0009", T, p)
        })
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
    } = new_vault();
    let mut stale = Vault::unlock(vault.keybag_bytes(), PASS, &mut Pins::default()).unwrap();
    vault
        .write_now(&mut pins, |v, p| v.add_recovery_wrapper(p))
        .unwrap();
    // `stale` still holds sequence 1; the device has seen sequence 2.
    let snapshot = pins.clone();
    assert_eq!(
        stale
            .write_now(&mut pins, |v, p| v.add_recovery_wrapper(p))
            .unwrap_err(),
        VaultError::Rollback { got: 2, seen: 2 }
    );
    assert_eq!(pins.highest, snapshot.highest);

    let mut foreign = new_vault().pins;
    assert_eq!(
        vault
            .write_now(&mut foreign, |v, p| v.add_recovery_wrapper(p))
            .unwrap_err(),
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
    assert!(
        vault
            .write_now(&mut pins, |v, p| v.remove_wrapper(&seed, p))
            .is_err()
    );
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
    } = new_vault();
    let before = vault.keybag_bytes().to_vec();
    let id = pass_id(&vault);
    assert_eq!(vault.below_policy_wrappers(), vec![id]);
    assert_eq!(
        vault
            .write_now(&mut pins, |v, p| v.rotate_vk(PASS, None, p))
            .unwrap_err(),
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
        .write_now(&mut pins, |v, p| {
            v.change_passcode(&id, "FAKE-policy-passcode-0010", floor, p)
        })
        .unwrap();
    assert!(vault.below_policy_wrappers().is_empty());
    let r = vault
        .write_now(&mut pins, |v, p| {
            v.rotate_vk("FAKE-policy-passcode-0010", None, p)
        })
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
    let mut vault = new_vault().vault;
    vault.keybag.sequence = u64::MAX;
    vault.bytes = vault.keybag.sign(&vault.signing);
    let before = vault.keybag_bytes().to_vec();
    let mut pins = Pins::default();
    assert_eq!(
        vault
            .write_now(&mut pins, |v, p| v.add_recovery_wrapper(p))
            .unwrap_err(),
        VaultError::SequenceExhausted
    );
    assert_eq!(vault.keybag_bytes(), &before[..]);
    assert!(pins.highest.is_none());
}

/// Clone the vault's passcode wrapper `n` extra times with `params` and fresh ids, and sign. With
/// `drop_recovery` the recovery wrapper is removed first, to make room for 63 passcode wrappers.
fn with_extra_passcode_wrappers(
    v: &Vault,
    n: usize,
    params: KdfParams,
    drop_recovery: bool,
) -> Vec<u8> {
    let mut kb: Keybag = v.keybag().clone();
    if drop_recovery {
        kb.wrappers.retain(|w| w.kind() != KIND_RECOVERY);
    }
    let base = kb
        .wrappers
        .iter()
        .find(|w| w.kind() == KIND_PASSCODE)
        .unwrap()
        .clone();
    for i in 0..n {
        let mut w = base.clone();
        w.id = [0x70 ^ i as u8; 16];
        if let WrapperBody::Passcode { params: p, .. } = &mut w.body {
            *p = params;
        }
        kb.wrappers.push(w);
    }
    kb.sequence += 1;
    kb.sign(&v.signing)
}

/// L2, probe R3: a signed keybag with 63 passcode wrappers at the format maximum would make one unlock
/// run 63 Argon2id passes of 1 GiB, t=10. Parse refuses it before any KDF work: at most
/// `MAX_PASSCODE_WRAPPERS` passcode wrappers, and their total `m_kib * t` at most one maximal wrapper.
#[test]
fn l2_unlock_kdf_cost_is_bounded_at_parse() {
    let v = new_vault().vault;
    let max = KdfParams {
        m_kib: 1_048_576,
        t: 10,
        p: 16,
    };
    max.check_format().unwrap();
    let r3 = with_extra_passcode_wrappers(&v, 62, max, true);
    assert!((8000..9000).contains(&r3.len()), "R3 shape: an 8 KB keybag");
    assert_eq!(
        Keybag::parse(&r3, &Pins::default()).unwrap_err(),
        VaultError::Malformed("too many passcode wrappers")
    );
    // Unlock refuses at parse, so it does no Argon2id run at all (this returns at once).
    assert!(Vault::unlock(&r3, PASS, &mut Pins::default()).is_err());

    // The count cap: four cheap wrappers parse, five do not.
    let four = with_extra_passcode_wrappers(&v, MAX_PASSCODE_WRAPPERS - 1, T, false);
    assert!(Keybag::parse(&four, &Pins::default()).is_ok());
    let five = with_extra_passcode_wrappers(&v, MAX_PASSCODE_WRAPPERS, T, false);
    assert_eq!(
        Keybag::parse(&five, &Pins::default()).unwrap_err(),
        VaultError::Malformed("too many passcode wrappers")
    );

    // The cost budget: one maximal wrapper next to a cheap one is over budget.
    let over = with_extra_passcode_wrappers(&v, 1, max, false);
    assert_eq!(
        Keybag::parse(&over, &Pins::default()).unwrap_err(),
        VaultError::KdfBounds
    );
    // A single maximal wrapper on its own is still within the budget (parse only; no KDF run here).
    let mut single = v.keybag().clone();
    for w in &mut single.wrappers {
        if let WrapperBody::Passcode { params, .. } = &mut w.body {
            *params = max;
        }
    }
    single.sequence += 1;
    assert!(Keybag::parse(&single.sign(&v.signing), &Pins::default()).is_ok());
}

/// L4: an empty passcode is refused when a passcode wrapper is written.
#[test]
fn l4_empty_passcode_is_refused() {
    assert_eq!(
        Vault::create_unchecked("", T).unwrap_err(),
        VaultError::PasscodeRejected("empty")
    );
    assert_eq!(
        Vault::create("", KdfParams::RECOMMENDED).unwrap_err(),
        VaultError::PasscodeRejected("empty")
    );
    let NewVault {
        mut vault,
        mut pins,
        ..
    } = new_vault();
    let id = pass_id(&vault);
    assert_eq!(
        vault
            .write_now(&mut pins, |v, p| v.change_passcode_unchecked(&id, "", T, p))
            .unwrap_err(),
        VaultError::PasscodeRejected("empty")
    );
    assert_eq!(vault.keybag().sequence, 1);
}

/// L4: passcodes with unassigned, private-use, noncharacter or control code points are refused when
/// set (NFKC of an unassigned code point is not stable across Unicode versions).
#[test]
fn l4_unassigned_private_use_and_control_code_points_are_refused() {
    for bad in [
        "FAKE-\u{0378}-unassigned",
        "FAKE-\u{E000}-private-use",
        "FAKE-\u{0007}-control",
        "FAKE-\u{10FFFF}-noncharacter",
    ] {
        assert_eq!(
            Vault::create_unchecked(bad, T).unwrap_err(),
            VaultError::PasscodeRejected("unassigned, private-use or control code point"),
            "passcode {} accepted",
            bad.escape_unicode()
        );
    }
    // Assigned non-ASCII is fine (a fullwidth letter NFKC-folds to ASCII), and unlocks by its NFKC twin.
    let nv = Vault::create_unchecked("FAKE-\u{FF21}\u{00E9}-ok", T).unwrap();
    assert!(
        Vault::unlock(
            nv.vault.keybag_bytes(),
            "FAKE-A\u{00E9}-ok",
            &mut Pins::default()
        )
        .is_ok()
    );
}

/// L6: an index whose payload generation differs from its blob generation is refused.
#[test]
fn l6_index_payload_generation_must_match_the_blob() {
    let NewVault {
        mut vault,
        mut pins,
        ..
    } = new_vault();
    let entries = vec![IndexEntry {
        name: "login/a-FAKE".into(),
        secret_type: crate::SecretType::Login,
        policy: crate::Policy::PerUse,
        revision: 1,
        last_used_unix: 0,
        tags: vec![],
        source: None,
    }];
    let gen1_payload = vault.encode_index(&entries).unwrap();
    vault
        .write_now(&mut pins, |v, p| v.rotate_vk_unchecked(PASS, None, p))
        .unwrap();
    // Sealed now, so the blob is generation 2 while the payload says generation 1.
    let odd = vault.seal_index(&gen1_payload, 2).unwrap();
    assert_eq!(
        vault.open_index(&odd.bytes).unwrap_err(),
        VaultError::Malformed("index generation differs from its blob generation")
    );
}
