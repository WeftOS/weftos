//! Regression tests for the round-3 crypto review (reports/review-vault-v1-crypto-r3-20261115.md), one
//! or more per finding, plus the commit-side branches that review listed as untested. All values are
//! fake; Argon2id runs the TEST profile.

use crate::VaultError;
use crate::blob;
use crate::hkdf_keys;
use crate::keybag::{KIND_SIGNSEED, Keybag, Pins, Wrapper, WrapperBody, wrapper_aad};
use crate::tests::{PASS, T, new_vault, req};
use crate::vault::Vault;
use crate::wrap::{PasscodeSeal, passcode_wrapper, seal48};

/// The store's object after the caller's `put_if_match`: what `commit_write` now compares against.
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

/// A keybag whose header and wrappers are all at `generation`, sealed under the vault's VK and signed
/// by its key, so only the structural rules can refuse it.
fn keybag_at_generation(v: &Vault, generation: u32) -> Keybag {
    let vid = v.keybag().vault_id;
    let pass = passcode_wrapper(PasscodeSeal {
        vault_id: &vid,
        id: [7u8; 16],
        vk: &v.vk,
        passcode: PASS,
        params: T,
        salt: [1u8; 16],
        nonce: [2u8; 24],
        generation,
    })
    .unwrap();
    let sk = hkdf_keys::signseed_key(&vid, &v.vk);
    let aad = wrapper_aad(&vid, &[9u8; 16], KIND_SIGNSEED, generation);
    let ct = seal48(&sk, &[3u8; 24], &aad, &v.signing.to_bytes()).unwrap();
    let mut kb = v.keybag().clone();
    kb.current_vk_generation = generation;
    kb.sequence += 1;
    kb.wrappers = vec![
        pass,
        Wrapper {
            id: [9u8; 16],
            vk_generation: generation,
            body: WrapperBody::SigningSeed {
                nonce: [3u8; 24],
                ct,
            },
        },
    ];
    kb
}

/// R3-L1, probe P1: a keybag at `current_vk_generation = 0`, or carrying a wrapper at generation 0, is
/// refused at parse and on write, so no device can record pins that `Pins::from_bytes` would refuse.
#[test]
fn r3_l1_generation_zero_is_refused() {
    let nv = new_vault();
    let zero = keybag_at_generation(&nv.vault, 0);
    let refused = VaultError::Malformed("vk generation is zero");
    assert_eq!(zero.validate_structure().unwrap_err(), refused);
    let bytes = zero.sign(&nv.vault.signing);
    assert_eq!(
        Keybag::parse(&bytes, &Pins::default()).unwrap_err(),
        refused
    );
    let mut pins = Pins::default();
    assert_eq!(Vault::unlock(&bytes, PASS, &mut pins).unwrap_err(), refused);
    assert_eq!(pins, Pins::default());

    // A generation-1 keybag that also carries a generation-0 wrapper (here a stray recovery wrapper).
    let mut stray = nv.vault.keybag().clone();
    let mut extra = stray.wrappers[1].clone();
    extra.id = [0x55; 16];
    extra.vk_generation = 0;
    stray.wrappers.push(extra);
    stray.sequence += 1;
    assert_eq!(stray.validate_structure().unwrap_err(), refused);
    assert_eq!(
        Keybag::parse(&stray.sign(&nv.vault.signing), &Pins::default()).unwrap_err(),
        refused
    );

    // Generation 1 built the same way still parses, and its pins round-trip.
    let one = keybag_at_generation(&nv.vault, 1).sign(&nv.vault.signing);
    let mut pins = Pins::default();
    Vault::unlock(&one, PASS, &mut pins).unwrap();
    assert_eq!(Pins::from_bytes(&pins.to_bytes()).unwrap(), pins);
}

/// R3-L2, probe P3: while the crate is DRAFT, a keybag the signer stripped of the DRAFT flag is
/// refused, so it can never unlock and take a real credential.
#[test]
fn r3_l2_reader_refuses_a_keybag_without_the_draft_flag() {
    let nv = new_vault();
    let mut kb = nv.vault.keybag().clone();
    kb.flags = 0;
    kb.sequence += 1;
    assert_eq!(kb.validate_structure().unwrap_err(), VaultError::NotDraft);
    let bytes = kb.sign(&nv.vault.signing);
    let mut pins = nv.pins.clone();
    assert_eq!(
        Keybag::parse(&bytes, &pins).unwrap_err(),
        VaultError::NotDraft
    );
    assert_eq!(
        Vault::unlock(&bytes, PASS, &mut pins).unwrap_err(),
        VaultError::NotDraft
    );
    assert_eq!(pins, nv.pins);
    let mut real = req("login/real-FAKE", b"FAKE");
    real.real_credential = true;
    assert_eq!(
        nv.vault.seal_secret(&real).unwrap_err(),
        VaultError::DraftRefusesRealCredentials
    );
}

/// R3-L3, probe P2: `commit_write` takes the bytes the store holds after `put_if_match` and refuses,
/// changing nothing, when they are not the pending keybag. The refused rotation's VK never becomes the
/// vault's, and the device then unlocks the store's keybag as usual.
#[test]
fn r3_l3_commit_refuses_when_the_store_holds_other_bytes() {
    let created = new_vault();
    let mut store = Store(created.vault.keybag_bytes().to_vec());
    let mut pins_a = Pins::default();
    let mut a = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    let mut pins_b = Pins::default();
    let mut b = Vault::unlock(&store.0, PASS, &mut pins_b).unwrap();

    // B wins the race.
    let pending_b = b.add_recovery_wrapper(&pins_b).unwrap();
    assert!(store.put_if_match(b.keybag_bytes(), pending_b.keybag_bytes()));
    b.commit_write(pending_b, &store.0, &mut pins_b).unwrap();

    // A's rotation is refused, and A's caller commits it anyway with the store's read-back.
    let before = (a.keybag_bytes().to_vec(), pins_a.clone());
    let pending_a = a.rotate_vk_unchecked(PASS, None, &pins_a).unwrap();
    assert!(!store.put_if_match(a.keybag_bytes(), pending_a.keybag_bytes()));
    assert_eq!(
        a.commit_write(pending_a, &store.0, &mut pins_a)
            .unwrap_err(),
        VaultError::StoreMismatch
    );
    assert_eq!((a.keybag_bytes().to_vec(), pins_a.clone()), before);
    let sealed = a.seal_secret(&req("login/a-FAKE", b"FAKE-a")).unwrap();
    assert_eq!(blob::peek_generation(&sealed.bytes).unwrap(), 1);

    let a2 = Vault::unlock(&store.0, PASS, &mut pins_a).unwrap();
    assert_eq!(
        &a2.open_named("login/a-FAKE", &sealed.bytes)
            .unwrap()
            .payload[..],
        b"FAKE-a"
    );
    assert_eq!(pins_a.highest(), pins_b.highest());
}

/// Commit-side `GenerationRollback`: the pins moved to a newer generation between prepare and commit
/// (a sibling instance rotated and committed with the same pins). The older write is refused.
#[test]
fn commit_refuses_a_write_the_pins_have_passed_by_generation() {
    let nv = new_vault();
    let bytes = nv.vault.keybag_bytes().to_vec();
    let mut pins = nv.pins.clone();
    let mut a = Vault::unlock(&bytes, PASS, &mut pins).unwrap();
    let mut b = Vault::unlock(&bytes, PASS, &mut pins).unwrap();
    let pending_a = a.add_recovery_wrapper(&pins).unwrap();
    let rot = b.rotate_vk_unchecked(PASS, None, &pins).unwrap();
    let rot_bytes = rot.keybag_bytes().to_vec();
    b.commit_write(rot, &rot_bytes, &mut pins).unwrap();
    assert_eq!(pins.vk_generation(), 2);

    let snapshot = (a.keybag_bytes().to_vec(), pins.clone());
    // A caller that also reports A's keybag as the store's read-back gets past the store check; the
    // generation pin still refuses it.
    let claimed = pending_a.keybag_bytes().to_vec();
    assert_eq!(
        a.commit_write(pending_a, &claimed, &mut pins).unwrap_err(),
        VaultError::GenerationRollback { got: 1, seen: 2 }
    );
    assert_eq!((a.keybag_bytes().to_vec(), pins), snapshot);
}

/// Commit-side `SignerMismatch`: the pins handed to `commit_write` belong to another vault's signer.
#[test]
fn commit_refuses_pins_of_another_signer() {
    let mut nv = new_vault();
    let other = new_vault();
    let mut wrong = other.pins.clone();
    let pending = nv.vault.add_recovery_wrapper(&nv.pins).unwrap();
    let stored = pending.keybag_bytes().to_vec();
    let before = nv.vault.keybag_bytes().to_vec();
    assert_eq!(
        nv.vault
            .commit_write(pending, &stored, &mut wrong)
            .unwrap_err(),
        VaultError::SignerMismatch
    );
    assert_eq!(
        (nv.vault.keybag_bytes(), &wrong),
        (&before[..], &other.pins)
    );
}
