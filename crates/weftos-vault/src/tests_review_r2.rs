//! Regression tests for the round-2 crypto review (reports/review-vault-v1-crypto-r2-20261115.md), one
//! or more per finding. All values are fake; Argon2id runs the TEST profile.

use crate::VaultError;
use crate::blob;
use crate::keybag::{FLAG_DRAFT, KIND_PASSCODE, Keybag, Pins};
use crate::pins::PINS_LEN;
use crate::tests::{PASS, T, new_vault, pass_id, req};
use crate::vault::Vault;

/// A store with `put_if_match`: the write lands only when the caller's base is still current.
struct Store(Vec<u8>);

impl Store {
    fn put_if_match(&mut self, base: &[u8], new: &[u8]) -> bool {
        if self.0 == base {
            self.0 = new.to_vec();
            true
        } else {
            false
        }
    }
}

/// Two devices, A and B, that have both unlocked the stored keybag (sequence 1, generation 1).
fn two_devices() -> (Store, Vault, Pins, Vault, Pins) {
    let store = Store(new_vault().vault.keybag_bytes().to_vec());
    let mut pins_a = Pins::default();
    let a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let mut pins_b = Pins::default();
    let b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    (store, a, pins_a, b, pins_b)
}

/// B writes `add_recovery_wrapper` and the store accepts it (sequence 2, generation 1).
fn b_writes_first(store: &mut Store, b: &mut Vault, pins_b: &mut Pins) {
    let pending = b.add_recovery_wrapper(pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pending.keybag_bytes()));
    b.commit_write(pending, pins_b).unwrap();
}

/// R2-M1, probe P1: A rotates, B wrote first, A's `put_if_match` fails. A drops the pending write; its
/// vault and pins are unchanged, the new VK sealed nothing, and A unlocks the store's keybag (no
/// `GenerationRollback`) and can rotate again on top of it.
#[test]
fn r2_m1_p1_failed_store_write_after_rotation_does_not_lock_the_device_out() {
    let (mut store, a, mut pins_a, mut b, mut pins_b) = two_devices();
    let before = (a.keybag_bytes().to_vec(), pins_a.clone());
    let pending = a.rotate_vk_unchecked(PASS, None, &pins_a).unwrap();
    assert_eq!(pending.report().new_generation, 2);
    // Until it is committed, the new VK is not the vault's: a secret sealed now is generation 1, which
    // the stored keybag can open.
    let sealed = a.seal_secret(&req("login/a-FAKE", b"FAKE-a")).unwrap();
    assert_eq!(blob::peek_generation(&sealed.bytes).unwrap(), 1);

    b_writes_first(&mut store, &mut b, &mut pins_b);
    assert!(!store.put_if_match(a.keybag_bytes(), pending.keybag_bytes()));
    drop(pending);
    assert_eq!((a.keybag_bytes().to_vec(), pins_a.clone()), before);

    let a2 = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    assert_eq!(
        a2.open_named("login/a-FAKE", &sealed.bytes)
            .unwrap()
            .payload[..],
        b"FAKE-a"[..]
    );
    // The rotation goes through on top of B's keybag.
    let mut a2 = a2;
    let pending = a2.rotate_vk_unchecked(PASS, None, &pins_a).unwrap();
    assert!(store.put_if_match(a2.keybag_bytes(), pending.keybag_bytes()));
    a2.commit_write(pending, &mut pins_a).unwrap();
    assert_eq!(
        (pins_a.highest().unwrap().sequence, pins_a.vk_generation()),
        (3, 2)
    );
    assert!(Vault::unlock(&store.0, PASS, &mut pins_b).is_ok());
}

/// R2-M1, probe P1b: the same without a rotation gives no `Fork` at sequence 2.
#[test]
fn r2_m1_p1b_failed_store_write_without_rotation_gives_no_fork() {
    let (mut store, a, mut pins_a, mut b, mut pins_b) = two_devices();
    let pending = a.add_recovery_wrapper(&pins_a).unwrap();
    b_writes_first(&mut store, &mut b, &mut pins_b);
    assert!(!store.put_if_match(a.keybag_bytes(), pending.keybag_bytes()));
    drop(pending);
    assert_eq!(pins_a.highest().unwrap().sequence, 1);
    let a2 = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    assert_eq!(a2.keybag_bytes(), &store.0[..]);
    assert_eq!(pins_a.highest(), pins_b.highest());
}

/// R2-M1: an accepted write advances the pins and installs the new keybag and VK; the store can then no
/// longer replay the keybag from before the write to this device.
#[test]
fn r2_m1_accepted_write_advances_the_pins() {
    let (mut store, mut a, mut pins_a, _, _) = two_devices();
    let before = store.0.clone();
    let pending = a.rotate_vk_unchecked(PASS, None, &pins_a).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pending.keybag_bytes()));
    let report = a.commit_write(pending, &mut pins_a).unwrap();
    assert_eq!(report.new_generation, 2);
    assert_eq!(a.keybag_bytes(), &store.0[..]);
    assert_eq!(
        (pins_a.highest().unwrap().sequence, pins_a.vk_generation()),
        (2, 2)
    );
    let sealed = a.seal_secret(&req("login/b-FAKE", b"FAKE-b")).unwrap();
    assert_eq!(blob::peek_generation(&sealed.bytes).unwrap(), 2);
    assert_eq!(
        Vault::unlock(&before, PASS, &mut pins_a).unwrap_err(),
        VaultError::GenerationRollback { got: 1, seen: 2 }
    );
    // The recovery key comes out of `commit_write`, after the store took the keybag.
    let pending = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pending.keybag_bytes()));
    let key = a.commit_write(pending, &mut pins_a).unwrap();
    assert!(Vault::unlock_with_recovery(&store.0, &key, &mut pins_a).is_ok());
}

/// R2-M1: `commit_write` refuses, with the vault and pins unchanged, a write built on other bytes, a
/// write from another vault, and a write the store did not take once the pins saw the store's keybag.
#[test]
fn r2_m1_commit_refuses_stale_foreign_and_superseded_writes() {
    let (mut store, mut a, mut pins_a, mut b, mut pins_b) = two_devices();
    let first = a.add_recovery_wrapper(&pins_a).unwrap();
    let second = a.add_recovery_wrapper(&pins_a).unwrap();
    a.commit_write(first, &mut pins_a).unwrap();
    let snapshot = (a.keybag_bytes().to_vec(), pins_a.clone());
    assert_eq!(
        a.commit_write(second, &mut pins_a).unwrap_err(),
        VaultError::StaleWrite
    );
    let other = new_vault();
    let foreign = other.vault.add_recovery_wrapper(&other.pins).unwrap();
    assert_eq!(
        a.commit_write(foreign, &mut pins_a).unwrap_err(),
        VaultError::StaleWrite
    );
    assert_eq!((a.keybag_bytes().to_vec(), pins_a.clone()), snapshot);

    // Device B: its write loses the race, it unlocks the store's keybag (A's sequence 2), and a buggy
    // caller then commits the refused write anyway. The pins catch the fork.
    let lost = b.add_recovery_wrapper(&pins_b).unwrap();
    store.0 = a.keybag_bytes().to_vec();
    Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pins_before = pins_b.clone();
    assert_eq!(
        b.commit_write(lost, &mut pins_b).unwrap_err(),
        VaultError::Fork { sequence: 2 }
    );
    assert_eq!(pins_b, pins_before);
}

/// R2-M2: every created vault carries the DRAFT flag, and writes keep it.
#[test]
fn r2_m2_a_vault_without_the_draft_flag_cannot_be_created() {
    let mut nv = new_vault();
    assert!(nv.vault.is_draft());
    assert_eq!(nv.vault.keybag().flags, FLAG_DRAFT);
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.rotate_vk_unchecked(PASS, None, p))
        .unwrap();
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.add_recovery_wrapper(p))
        .unwrap();
    assert_eq!(nv.vault.keybag().flags, FLAG_DRAFT);
    let reopened = Vault::unlock(nv.vault.keybag_bytes(), PASS, &mut Pins::default()).unwrap();
    assert!(reopened.is_draft());
}

/// R2-L1: pins are read through getters and persisted as bytes; loading is strict.
#[test]
fn r2_l1_pins_round_trip_as_bytes_and_loading_is_strict() {
    let nv = new_vault();
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

/// R2-L2: a replayed stale revision of an old-generation blob is not moved forward by re-seal.
#[test]
fn r2_l2_reseal_refuses_a_replayed_stale_revision() {
    let mut nv = new_vault();
    let mut r = req("login/a-FAKE", b"FAKE-old");
    let stale = nv.vault.seal_secret(&r).unwrap();
    r.payload = b"FAKE-new";
    r.revision = 2;
    let latest = nv.vault.seal_secret(&r).unwrap();
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.rotate_vk_unchecked(PASS, None, p))
        .unwrap();
    assert_eq!(blob::peek_generation(&stale.bytes).unwrap(), 1);
    // The index says revision 2; the store replays revision 1 during the lazy pass.
    assert_eq!(
        nv.vault
            .reseal_secret("login/a-FAKE", &stale.bytes, 2)
            .unwrap_err(),
        VaultError::StaleRevision {
            got: 1,
            expected: 2
        }
    );
    assert!(
        nv.vault
            .reseal_all(&[("login/a-FAKE", &stale.bytes[..], 2)])
            .is_err()
    );
    let moved = nv
        .vault
        .reseal_secret("login/a-FAKE", &latest.bytes, 2)
        .unwrap()
        .unwrap();
    let o = nv
        .vault
        .open_named("login/a-FAKE", &moved.new_secret.bytes)
        .unwrap();
    assert_eq!((&o.payload[..], o.revision), (&b"FAKE-new"[..], 3));
}

/// R2-L3, probe P4: invisible code points and whitespace-only passcodes are refused when set.
#[test]
fn r2_l3_invisible_and_whitespace_only_passcodes_are_refused() {
    for bad in [
        "FAKE-\u{200B}-zwsp",
        "FAKE-\u{200C}-zwnj",
        "FAKE-\u{200D}-zwj",
        "FAKE-\u{00AD}-soft-hyphen",
        "FAKE-\u{FE0F}-variation-selector",
        "FAKE-\u{E0001}-language-tag",
        "FAKE-\u{E0041}-tag-letter",
        "FAKE-\u{2028}-line-separator",
        "FAKE-\u{2029}-paragraph-separator",
        "FAKE-\u{202E}-rlo",
        "FAKE-\u{2066}-lri",
        "FAKE-\u{200F}-rlm",
        "FAKE-\u{FEFF}-bom",
    ] {
        assert_eq!(
            Vault::create_unchecked(bad, T).unwrap_err(),
            VaultError::PasscodeRejected("invisible code point"),
            "passcode {} accepted",
            bad.escape_unicode()
        );
    }
    for blank in [" ", " \t ", "\u{3000}"] {
        assert_eq!(
            Vault::create_unchecked(blank, T).unwrap_err(),
            VaultError::PasscodeRejected("whitespace only")
        );
    }
    // The rules apply on change too; a passcode with an inner space is fine.
    let nv = new_vault();
    let id = pass_id(&nv.vault);
    assert_eq!(
        nv.vault
            .change_passcode_unchecked(&id, "FAKE-\u{200B}", T, &nv.pins)
            .unwrap_err(),
        VaultError::PasscodeRejected("invisible code point")
    );
    assert!(Vault::create_unchecked("FAKE passcode with spaces", T).is_ok());
}

/// R2-L4, probe P3: a keybag whose only passcode wrapper is at an older generation is refused.
#[test]
fn r2_l4_keybag_without_a_current_generation_passcode_wrapper_is_refused() {
    let mut nv = new_vault();
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.rotate_vk_unchecked(PASS, None, p))
        .unwrap();
    let mut kb: Keybag = nv.vault.keybag().clone();
    for w in &mut kb.wrappers {
        if w.kind() == KIND_PASSCODE {
            w.vk_generation = 1;
        }
    }
    kb.sequence += 1;
    let bytes = kb.sign(&nv.vault.signing);
    assert_eq!(
        Keybag::parse(&bytes, &Pins::default()).unwrap_err(),
        VaultError::Malformed("no current-generation passcode wrapper")
    );
    assert_eq!(
        kb.validate_structure().unwrap_err(),
        VaultError::Malformed("no current-generation passcode wrapper")
    );
}
