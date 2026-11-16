//! Review lows F-L1 to F-L4 of `reports/review-vault-kdf-floor-20261115.md`: the reviewer's probes PF1 to PF3,
//! PF5 and PF6 promoted to tests, plus the accepted-write-landed helper (`PendingWrite::landed_in`).
//! All values are fake.

use crate::VaultError;
use crate::keybag::{KIND_RECOVERY, Keybag, Pins, Wrapper};
use crate::passcode_kdf::KdfParams;
use crate::tests::{PASS, new_vault, pass_id, req};
use crate::vault::Vault;
use crate::wrap::{PasscodeSeal, passcode_wrapper};

/// A store with compare-and-swap, as the contract assumes.
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

fn pass_w(v: &Vault, id: [u8; 16], pass: &str, params: KdfParams) -> Wrapper {
    passcode_wrapper(PasscodeSeal {
        vault_id: &v.keybag().vault_id,
        id,
        vk: &v.vk,
        passcode: pass,
        params,
        salt: [4u8; 16],
        nonce: [5u8; 24],
        generation: v.keybag().current_vk_generation,
    })
    .unwrap()
}

/// F-L1 (PF3): rotation refuses only when the given passcode opens a below-floor wrapper. A weak wrapper
/// of another passcode is dropped, the rotation succeeds, and nothing weak is carried.
#[test]
fn rotation_drops_a_weak_wrapper_of_another_passcode() {
    let nv = new_vault(); // its passcode wrapper is weak (TEST profile), passcode PASS
    let strong = pass_w(
        &nv.vault,
        [0x33; 16],
        "FAKE-strong-0001",
        KdfParams::RECOMMENDED,
    );
    let mut kb: Keybag = nv.vault.keybag().clone();
    kb.wrappers.push(strong);
    kb.sequence += 1;
    let bytes = kb.sign(&nv.vault.signing);
    let mut pins = Pins::default();
    let mut v = Vault::unlock(&bytes, "FAKE-strong-0001", &mut pins).unwrap();
    let weak = pass_id(&nv.vault);
    assert_eq!(v.below_policy_wrappers(), vec![weak]);
    let r = v
        .write_now(&mut pins, |v, p| v.rotate_vk("FAKE-strong-0001", None, p))
        .unwrap();
    assert_eq!((r.passcode_resealed, r.dropped >= 1), (1, true));
    assert!(v.keybag().wrapper(&weak).is_none());
    assert!(v.below_policy_wrappers().is_empty());
    // The weak passcode can no longer unlock the new keybag.
    assert_eq!(
        Vault::unlock(v.keybag_bytes(), PASS, &mut pins.clone()).unwrap_err(),
        VaultError::UnlockFailed
    );
}

/// F-L1 (PF3, other direction): the passcode that opens the weak wrapper is refused, whatever else exists.
#[test]
fn rotation_refuses_when_the_given_passcode_opens_a_weak_wrapper() {
    let nv = new_vault();
    let strong = pass_w(
        &nv.vault,
        [0x33; 16],
        "FAKE-strong-0001",
        KdfParams::RECOMMENDED,
    );
    let mut kb: Keybag = nv.vault.keybag().clone();
    kb.wrappers.push(strong);
    kb.sequence += 1;
    let bytes = kb.sign(&nv.vault.signing);
    let mut pins = Pins::default();
    let v = Vault::unlock(&bytes, PASS, &mut pins).unwrap();
    assert_eq!(
        v.rotate_vk(PASS, None, &pins).unwrap_err(),
        VaultError::KdfPolicy
    );
}

/// F-L1 (PF6): `rotate_vk` derives with the stored parameters before the policy check, so the policy answer
/// follows a successful open. A wrong passcode is `UnlockFailed`, the right one `KdfPolicy`.
#[test]
fn rotation_policy_check_follows_the_kdf() {
    let nv = new_vault();
    assert_eq!(
        nv.vault
            .rotate_vk("FAKE-wrong-0003", None, &nv.pins)
            .unwrap_err(),
        VaultError::UnlockFailed
    );
    assert_eq!(
        nv.vault.rotate_vk(PASS, None, &nv.pins).unwrap_err(),
        VaultError::KdfPolicy
    );
}

/// F-L4 (PF5): every carrying write keeps the legacy wrapper byte for byte; removing the only (weak)
/// passcode wrapper is refused; every write keeps DRAFT and generation 1 or more.
#[test]
fn carrying_writes_keep_the_legacy_wrapper_and_never_self_brick() {
    let mut nv = new_vault();
    let id = pass_id(&nv.vault);
    let body = nv.vault.keybag().wrapper(&id).unwrap().body.clone();
    let dev = nv
        .vault
        .write_now(&mut nv.pins, |v, p| {
            v.add_device_wrapper(1, b"FAKE-ref", |_, _, _| Ok(vec![1, 2, 3]), p)
        })
        .unwrap();
    assert_eq!(nv.vault.keybag().wrapper(&id).unwrap().body, body);
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.remove_wrapper(&dev, p))
        .unwrap();
    let rec = nv
        .vault
        .keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == KIND_RECOVERY)
        .unwrap()
        .id;
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.remove_wrapper(&rec, p))
        .unwrap();
    assert_eq!(nv.vault.keybag().wrapper(&id).unwrap().body, body);
    assert!(
        nv.vault
            .write_now(&mut nv.pins, |v, p| v.remove_wrapper(&id, p))
            .is_err()
    );
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.rotate_vk_unchecked(PASS, None, p))
        .unwrap();
    nv.vault
        .write_now(&mut nv.pins, |v, p| v.drop_history(1, p))
        .unwrap();
    let bytes = nv.vault.keybag_bytes().to_vec();
    let v = Vault::unlock(&bytes, PASS, &mut nv.pins).unwrap();
    assert!(v.is_draft());
    assert_eq!(v.keybag().current_vk_generation, 2);
}

/// F-L2 (PF1): `commit_write` compares the bytes it is GIVEN. A caller that echoes the pending bytes after a
/// refused `put_if_match` commits a write the store never took, so the check is not a store attestation and
/// `stored` must come from a fresh GET. The pins then refuse the store's keybag (the R3-L3 failure).
#[test]
fn store_mismatch_is_not_a_store_attestation() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins_a = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();

    let pa = a.rotate_vk_unchecked(PASS, None, &pins_a).unwrap();
    assert!(!store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    // The honest read-back is the store's keybag, which is not the pending one.
    assert_eq!(
        a.commit_write(pa, &store.0, &mut pins_a).unwrap_err(),
        VaultError::StoreMismatch
    );
    // Echoing the request body passes the check: the commit cannot tell.
    let pa = a.rotate_vk_unchecked(PASS, None, &pins_a).unwrap();
    let echo = pa.keybag_bytes().to_vec();
    a.commit_write(pa, &echo, &mut pins_a).unwrap();
    assert_eq!(a.keybag().current_vk_generation, 2);
    assert!(matches!(
        Vault::unlock(&store.0, PASS, &mut pins_a).unwrap_err(),
        VaultError::GenerationRollback { .. }
            | VaultError::Rollback { .. }
            | VaultError::Fork { .. }
    ));
    let s = a.seal_secret(&req("login/x-FAKE", b"FAKE")).unwrap();
    let b2 = Vault::unlock(&store.0, PASS, &mut Pins::default()).unwrap();
    assert!(b2.open_named("login/x-FAKE", &s.bytes).is_err());
}

/// F-L3 (PF2): the write landed, another device wrote on top, and the read-back mismatches.
/// `landed_in` recognises the accepted write from the store's later keybag, so the caller can tell the
/// user the recovery key is lost rather than say nothing.
#[test]
fn landed_in_recognises_an_accepted_write_the_read_back_raced() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins_a = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let pa = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    // The exact read-back matches.
    assert!(pa.landed_in(&store.0, &pins_a).unwrap());
    // B reads the new keybag and writes on top before A reads back.
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();
    assert!(pa.landed_in(&store.0, &pins_a).unwrap());
    assert_eq!(
        a.commit_write(pa, &store.0, &mut pins_a).unwrap_err(),
        VaultError::StoreMismatch
    );
}

/// F-L3: a write the store refused does not read as landed, even when another device's keybag sits at or
/// past the pending sequence; nor does bytes that do not verify.
#[test]
fn landed_in_is_false_for_a_refused_write_and_errors_on_bad_bytes() {
    let nv = new_vault();
    let mut store = Store(nv.vault.keybag_bytes().to_vec());
    let mut pins_a = Pins::default();
    let a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();
    let pb = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pb.keybag_bytes()));
    b.commit_write(pb, &store.0, &mut pins_b).unwrap();
    // A lost the race: its recovery wrapper is not in the store.
    let pa = a.add_recovery_wrapper(&pins_a).unwrap();
    assert!(!store.put_if_match(a.keybag_bytes(), pa.keybag_bytes()));
    assert!(!pa.landed_in(&store.0, &pins_a).unwrap());
    // Corrupt bytes do not verify.
    let mut bad = store.0.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    assert_eq!(
        pa.landed_in(&bad, &pins_a).unwrap_err(),
        VaultError::BadSignature
    );
    // A removal adds no wrapper to recognise, so only the exact keybag counts as landed.
    let rec = b
        .keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == KIND_RECOVERY)
        .unwrap()
        .id;
    let pr = b.remove_wrapper(&rec, &pins_b).unwrap();
    assert!(!pr.landed_in(&store.0, &pins_b).unwrap());
    assert!(pr.landed_in(pr.keybag_bytes(), &pins_b).unwrap());
}
