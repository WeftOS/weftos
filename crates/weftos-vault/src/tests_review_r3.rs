//! Regression tests for the round-3 crypto review (reports/review-vault-v1-crypto-r3-20261115.md), one
//! or more per finding, plus the commit-side branches that review listed as untested. All values are
//! fake; Argon2id runs the TEST profile.

use crate::VaultError;
use crate::hkdf_keys;
use crate::keybag::{KIND_SIGNSEED, Keybag, Pins, Wrapper, WrapperBody, wrapper_aad};
use crate::tests::{PASS, T, new_vault, req};
use crate::vault::Vault;
use crate::wrap::{PasscodeSeal, passcode_wrapper, seal48};

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
