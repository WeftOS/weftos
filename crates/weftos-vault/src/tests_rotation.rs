//! Vault-key rotation tests (spec 9.4). All values are fake; Argon2id runs the TEST profile.

use crate::VaultError;
use crate::blob;
use crate::keybag::{KIND_HISTORY, Keybag, Pins, WrapperBody};
use crate::tests::{PASS, new_vault, req};
use crate::vault::{NewVault, Vault};

fn gen_of(blob: &[u8]) -> u32 {
    blob::peek_generation(blob).unwrap()
}

#[test]
fn rotate_then_old_blobs_still_open_and_new_blobs_use_the_new_generation() {
    let NewVault {
        mut vault,
        recovery_key,
        ..
    } = new_vault(true);
    let a = vault.seal_secret(&req("login/a", b"FAKE-a-0001")).unwrap();
    let b = vault.seal_secret(&req("token/b", b"FAKE-b-0002")).unwrap();
    assert_eq!(gen_of(&a.bytes), 1);

    let r = vault
        .rotate_vk(PASS, Some(&recovery_key), &mut Pins::default())
        .unwrap();
    assert_eq!(
        (
            r.new_generation,
            r.passcode_resealed,
            r.recovery_resealed,
            r.dropped,
            r.history_entries
        ),
        (2, 1, 1, 0, 1)
    );
    assert_eq!(vault.keybag().current_vk_generation, 2);

    // Old blobs open by walking the history, by name and by slot.
    let oa = vault.open_named("login/a", &a.bytes).unwrap();
    assert_eq!(&oa.payload[..], b"FAKE-a-0001");
    assert_eq!(
        &vault.open_secret(&b.slot, &b.bytes).unwrap().payload[..],
        b"FAKE-b-0002"
    );
    // The old blob sits at an old-generation slot, not the new one.
    assert_ne!(vault.slot_for_name("login/a").unwrap(), a.slot);

    // New blobs use generation 2 and the new slot.
    let c = vault.seal_secret(&req("login/a", b"FAKE-a-0003")).unwrap();
    assert_eq!(gen_of(&c.bytes), 2);
    assert_eq!(c.slot, vault.slot_for_name("login/a").unwrap());
    assert_eq!(
        &vault.open_named("login/a", &c.bytes).unwrap().payload[..],
        b"FAKE-a-0003"
    );

    // A fresh unlock (passcode, then recovery key) sees the same history.
    let bytes = vault.keybag_bytes().to_vec();
    let v2 = Vault::unlock(&bytes, PASS, &mut Pins::default()).unwrap();
    assert_eq!(
        &v2.open_named("token/b", &b.bytes).unwrap().payload[..],
        b"FAKE-b-0002"
    );
    let v3 = Vault::unlock_with_recovery(&bytes, &recovery_key, &mut Pins::default()).unwrap();
    assert_eq!(
        &v3.open_named("login/a", &a.bytes).unwrap().payload[..],
        b"FAKE-a-0001"
    );
}

#[test]
fn two_rotations_keep_every_generation_reachable() {
    let mut vault = new_vault(true).vault;
    let g1 = vault.seal_secret(&req("login/a", b"FAKE-g1")).unwrap();
    vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();
    let g2 = vault.seal_secret(&req("login/a", b"FAKE-g2")).unwrap();
    let r = vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();
    assert_eq!((r.new_generation, r.history_entries), (3, 2));
    assert_eq!(vault.history_generations(), vec![1, 2]);
    let g3 = vault.seal_secret(&req("login/a", b"FAKE-g3")).unwrap();
    assert_eq!(gen_of(&g3.bytes), 3);

    let bytes = vault.keybag_bytes().to_vec();
    let v = Vault::unlock(&bytes, PASS, &mut Pins::default()).unwrap();
    for (blob, want) in [(&g1, "FAKE-g1"), (&g2, "FAKE-g2"), (&g3, "FAKE-g3")] {
        assert_eq!(
            v.open_named("login/a", &blob.bytes)
                .unwrap()
                .payload
                .as_slice(),
            want.as_bytes()
        );
    }
    // Unknown generation: no key, no guess.
    let mut odd = g1.bytes.clone();
    odd[6..10].copy_from_slice(&9u32.to_be_bytes());
    assert_eq!(
        v.open_secret(&g1.slot, &odd).unwrap_err(),
        VaultError::GenerationMismatch(9)
    );
}

#[test]
fn a_tampered_history_entry_fails() {
    let mut vault = new_vault(true).vault;
    let old = vault.seal_secret(&req("login/a", b"FAKE-old")).unwrap();
    vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();
    let signed = vault.keybag_bytes().to_vec();

    // Flipping a byte of the entry without re-signing breaks the keybag signature.
    let kb = Keybag::parse(&signed, &Pins::default()).unwrap();
    let flip = signed
        .windows(48)
        .position(|w| {
            matches!(&kb.wrappers.iter().find(|x| x.kind() == KIND_HISTORY).unwrap().body,
            WrapperBody::KeyHistory { ct, .. } if &ct[..] == w)
        })
        .unwrap();
    let mut bad = signed.clone();
    bad[flip] ^= 1;
    assert_eq!(
        Vault::unlock(&bad, PASS, &mut Pins::default()).unwrap_err(),
        VaultError::BadSignature
    );

    // A signer who re-signs a corrupted entry still cannot make it open: the AEAD tag fails.
    let mut forged = vault.keybag().clone();
    for w in &mut forged.wrappers {
        if let WrapperBody::KeyHistory { ct, .. } = &mut w.body {
            ct[0] ^= 1;
        }
    }
    forged.sequence += 1;
    let bytes = forged.sign(&vault.signing);
    let v = Vault::unlock(&bytes, PASS, &mut Pins::default()).unwrap();
    assert_eq!(
        v.open_named("login/a", &old.bytes).unwrap_err(),
        VaultError::AuthFailed
    );

    // A history entry with a different nonce fails: the nonce is part of the seal.
    let mut moved = vault.keybag().clone();
    for w in &mut moved.wrappers {
        if let WrapperBody::KeyHistory { nonce, .. } = &mut w.body {
            nonce[0] ^= 1;
        }
    }
    moved.sequence += 1;
    let v = Vault::unlock(&moved.sign(&vault.signing), PASS, &mut Pins::default()).unwrap();
    assert_eq!(
        v.open_named("login/a", &old.bytes).unwrap_err(),
        VaultError::AuthFailed
    );
}

#[test]
fn rotation_bumps_the_sequence_and_resigns() {
    let mut vault = new_vault(true).vault;
    let before = vault.keybag_bytes().to_vec();
    let kb0 = vault.keybag().clone();
    vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();
    let after = vault.keybag_bytes().to_vec();
    assert_ne!(before, after);
    assert_eq!(vault.keybag().sequence, kb0.sequence + 1);

    // Same signer; a device that pinned the old keybag accepts the new one and then refuses the old.
    let mut pins = Pins::default();
    Vault::unlock(&before, PASS, &mut pins).unwrap();
    let kb1 = Keybag::parse(&after, &pins).unwrap();
    assert_eq!(kb1.sign_pub, kb0.sign_pub);
    let mut pins2 = Pins::default();
    Vault::unlock(&after, PASS, &mut pins2).unwrap();
    assert!(matches!(
        Keybag::parse(&before, &pins2),
        Err(VaultError::Rollback { .. })
    ));

    // Exactly one signing-seed wrapper, at the new generation, and the vault_id is unchanged.
    assert_eq!(kb1.vault_id, kb0.vault_id);
    assert_eq!(kb1.current_vk_generation, 2);
    kb1.validate_structure().unwrap();
}

#[test]
fn a_wrong_passcode_changes_nothing() {
    let mut vault = new_vault(true).vault;
    let before = vault.keybag_bytes().to_vec();
    assert_eq!(
        vault
            .rotate_vk("FAKE-wrong-passcode", None, &mut Pins::default())
            .unwrap_err(),
        VaultError::UnlockFailed
    );
    assert_eq!(vault.keybag_bytes(), &before[..]);
    assert_eq!(vault.keybag().current_vk_generation, 1);
}

#[test]
fn recovery_wrapper_is_dropped_without_its_key() {
    let NewVault {
        mut vault,
        recovery_key,
        ..
    } = new_vault(true);
    let r = vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();
    assert_eq!((r.recovery_resealed, r.dropped), (0, 1));
    let bytes = vault.keybag_bytes().to_vec();
    assert_eq!(
        Vault::unlock_with_recovery(&bytes, &recovery_key, &mut Pins::default()).unwrap_err(),
        VaultError::UnlockFailed
    );
    // A fresh recovery key can be added afterwards.
    let k2 = vault.add_recovery_wrapper(&mut Pins::default()).unwrap();
    let bytes = vault.keybag_bytes().to_vec();
    assert!(Vault::unlock_with_recovery(&bytes, &k2, &mut Pins::default()).is_ok());
}

#[test]
fn reseal_moves_a_secret_to_the_new_generation_with_a_tombstone() {
    let mut vault = new_vault(true).vault;
    let old = vault.seal_secret(&req("login/a", b"FAKE-a-0001")).unwrap();
    vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();

    let r = vault.reseal_secret("login/a", &old.bytes).unwrap().unwrap();
    assert_eq!(r.old_slot, old.slot);
    assert_eq!(r.new_secret.slot, vault.slot_for_name("login/a").unwrap());
    assert_ne!(r.new_secret.slot, old.slot);
    assert_eq!(gen_of(&r.new_secret.bytes), 2);
    let o = vault.open_named("login/a", &r.new_secret.bytes).unwrap();
    assert_eq!((&o.payload[..], o.revision), (&b"FAKE-a-0001"[..], 2));

    // Delete marker: t/<old hash>, type 3, empty payload, current generation.
    assert_eq!(r.tombstone.slot, format!("t/{}", &old.slot[2..]));
    let t = vault
        .open_secret(&r.tombstone.slot, &r.tombstone.bytes)
        .unwrap();
    assert!(t.payload.is_empty());
    assert_eq!(gen_of(&r.tombstone.bytes), 2);

    // Already current: nothing to do. reseal_all skips current blobs and handles old ones.
    assert!(
        vault
            .reseal_secret("login/a", &r.new_secret.bytes)
            .unwrap()
            .is_none()
    );
    let all = vault
        .reseal_all(&[
            ("login/a", &old.bytes[..]),
            ("login/a", &r.new_secret.bytes[..]),
        ])
        .unwrap();
    assert_eq!(all.len(), 1);

    // A wrong name cannot re-seal.
    assert_eq!(
        vault.reseal_secret("login/zzz", &old.bytes).unwrap_err(),
        VaultError::AuthFailed
    );
}

#[test]
fn history_can_be_dropped_after_the_pass_and_then_old_blobs_stop_opening() {
    let mut vault = new_vault(true).vault;
    let old = vault.seal_secret(&req("login/a", b"FAKE-a-0001")).unwrap();
    vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();
    let seq = vault.keybag().sequence;
    vault.drop_history(1, &mut Pins::default()).unwrap();
    assert_eq!(vault.keybag().sequence, seq + 1);
    assert!(vault.history_generations().is_empty());
    assert_eq!(
        vault.open_named("login/a", &old.bytes).unwrap_err(),
        VaultError::GenerationMismatch(1)
    );
    assert!(vault.drop_history(1, &mut Pins::default()).is_err());
}

#[test]
fn history_structure_rules_are_enforced() {
    let mut vault = new_vault(true).vault;
    vault.rotate_vk(PASS, None, &mut Pins::default()).unwrap();
    let hist = vault
        .keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == KIND_HISTORY)
        .unwrap()
        .clone();

    let mut dup = vault.keybag().clone();
    let mut again = hist.clone();
    again.id = [9; 16];
    dup.wrappers.push(again);
    assert!(
        dup.validate_structure().is_err(),
        "two entries for one generation"
    );

    let mut future = vault.keybag().clone();
    let mut w = hist;
    w.id = [8; 16];
    w.vk_generation = future.current_vk_generation;
    future.wrappers.push(w);
    assert!(
        future.validate_structure().is_err(),
        "history for the current generation"
    );
}
