//! Behavioural tests. All values are fake. Argon2id runs with the TEST profile (8 MiB, t=1);
//! the full-cost profile is exercised only in `vectors`.

use crate::VaultError;
use crate::blob::{Policy, SecretType};
use crate::keybag::{KIND_SIGNSEED, Keybag, Pins, Wrapper, WrapperBody, sign_bytes};
use crate::passcode_kdf::KdfParams;
use crate::secrets::SealRequest;
use crate::vault::{NewVault, Vault};

pub(crate) fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

pub(crate) const PASS: &str = "FAKE-passcode-for-tests-0001";
pub(crate) const T: KdfParams = KdfParams::TEST;

pub(crate) fn new_vault(draft: bool) -> NewVault {
    Vault::create_unchecked(PASS, T, draft).unwrap()
}

pub(crate) fn req<'a>(name: &'a str, payload: &'a [u8]) -> SealRequest<'a> {
    SealRequest {
        name,
        secret_type: SecretType::Login,
        policy: Policy::PerUse,
        payload,
        revision: 1,
        real_credential: false,
    }
}

pub(crate) fn pass_id(v: &Vault) -> [u8; 16] {
    v.keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == 1)
        .unwrap()
        .id
}

/// Re-encode the vault's keybag, let `f` patch the unsigned bytes, and re-sign with the vault's key.
fn forge(v: &Vault, f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut u = v.keybag().unsigned_bytes();
    f(&mut u);
    sign_bytes(&u, &v.signing)
}

#[test]
fn create_unlock_roundtrip_and_wrong_passcode() {
    let nv = new_vault(true);
    let bytes = nv.vault.keybag_bytes().to_vec();
    assert!(Vault::unlock(&bytes, PASS, &mut Pins::default()).is_ok());
    assert_eq!(
        Vault::unlock(&bytes, "FAKE-wrong-passcode", &mut Pins::default()).unwrap_err(),
        VaultError::UnlockFailed
    );
}

#[test]
fn new_wrapper_policy_is_enforced_on_the_public_path() {
    assert_eq!(
        Vault::create(PASS, T, true).unwrap_err(),
        VaultError::KdfPolicy
    );
    assert_eq!(
        KdfParams {
            m_kib: 19456,
            t: 1,
            p: 1
        }
        .check_new_policy(),
        Err(VaultError::KdfPolicy)
    );
    assert!(
        KdfParams {
            m_kib: 46080,
            t: 1,
            p: 1
        }
        .check_new_policy()
        .is_ok()
    );
    assert!(KdfParams::RECOMMENDED.check_new_policy().is_ok());
    assert_eq!(
        KdfParams {
            m_kib: 1,
            t: 1,
            p: 1
        }
        .check_new_policy(),
        Err(VaultError::KdfBounds)
    );
    assert_eq!(
        KdfParams {
            m_kib: 8192,
            t: 11,
            p: 1
        }
        .check_format(),
        Err(VaultError::KdfBounds)
    );
    assert_eq!(
        KdfParams {
            m_kib: 8192,
            t: 1,
            p: 17
        }
        .check_format(),
        Err(VaultError::KdfBounds)
    );
}

#[test]
fn passcode_change_rewraps_only_one_wrapper_and_bumps_sequence() {
    let mut nv = new_vault(true);
    let old = nv.vault.keybag_bytes().to_vec();
    let id = pass_id(&nv.vault);
    let sealed = nv.vault.seal_secret(&req("login/a", b"FAKE-one")).unwrap();
    let before = nv.vault.keybag().clone();
    let new = nv
        .vault
        .change_passcode_unchecked(&id, "FAKE-new-passcode-0002", T, &mut Pins::default())
        .unwrap()
        .to_vec();
    let after = nv.vault.keybag();
    assert_eq!(after.sequence, 2);
    assert_ne!(old, new);
    // Recovery and signing wrappers are byte-identical; the passcode wrapper changed.
    assert_eq!(before.wrappers[1], after.wrappers[1]);
    assert_eq!(before.wrappers[2], after.wrappers[2]);
    assert_ne!(before.wrappers[0], after.wrappers[0]);
    assert_eq!(
        Vault::unlock(&new, PASS, &mut Pins::default()).unwrap_err(),
        VaultError::UnlockFailed
    );
    let v = Vault::unlock(&new, "FAKE-new-passcode-0002", &mut Pins::default()).unwrap();
    // Blobs are untouched by a passcode change.
    assert_eq!(
        &v.open_named("login/a", &sealed.bytes).unwrap().payload[..],
        b"FAKE-one"
    );
}

#[test]
fn rolled_back_keybag_is_refused_and_forks_are_detected() {
    let mut nv = new_vault(true);
    let v1 = nv.vault.keybag_bytes().to_vec();
    let id = pass_id(&nv.vault);
    let v2 = nv
        .vault
        .change_passcode_unchecked(&id, "FAKE-two", T, &mut Pins::default())
        .unwrap()
        .to_vec();

    let mut pins = Pins::default();
    Vault::unlock(&v2, "FAKE-two", &mut pins).unwrap();
    assert_eq!(
        Vault::unlock(&v1, PASS, &mut pins).unwrap_err(),
        VaultError::Rollback { got: 1, seen: 2 }
    );
    // Re-presenting the same sequence-2 bytes is fine.
    assert!(Vault::unlock(&v2, "FAKE-two", &mut pins).is_ok());

    // A second, different keybag at sequence 2 (built from the same v1) is a fork.
    let mut other = Vault::unlock(&v1, PASS, &mut Pins::default()).unwrap();
    let v2b = other
        .change_passcode_unchecked(&id, "FAKE-two", T, &mut Pins::default())
        .unwrap()
        .to_vec();
    assert_ne!(v2, v2b);
    assert_eq!(
        Vault::unlock(&v2b, "FAKE-two", &mut pins).unwrap_err(),
        VaultError::Fork { sequence: 2 }
    );
}

#[test]
fn pinned_signer_mismatch_is_refused() {
    let a = new_vault(true);
    let b = new_vault(true);
    let mut pins = Pins::default();
    Vault::unlock(a.vault.keybag_bytes(), PASS, &mut pins).unwrap();
    assert_eq!(
        Vault::unlock(b.vault.keybag_bytes(), PASS, &mut pins).unwrap_err(),
        VaultError::SignerMismatch
    );
}

#[test]
fn escrow_node_and_unknown_wrapper_kinds_are_rejected() {
    let nv = new_vault(true);
    for kind in [0u8, 6, 7, 8, 255] {
        let forged = forge(&nv.vault, |u| u[86] = kind);
        assert_eq!(
            Keybag::parse(&forged, &Pins::default()).unwrap_err(),
            VaultError::RejectedWrapperKind(kind)
        );
    }
}

#[test]
fn reserved_platforms_and_other_structure_violations_are_rejected() {
    let nv = new_vault(true);
    let v = &nv.vault;
    let signed_with = |mutate: &dyn Fn(&mut Keybag)| {
        let mut kb = v.keybag().clone();
        mutate(&mut kb);
        kb.sign(&v.signing)
    };
    for platform in [0u8, 3, 4, 5, 9] {
        let b = signed_with(&|kb| {
            kb.wrappers.push(Wrapper {
                id: [9; 16],
                vk_generation: 1,
                body: WrapperBody::Device {
                    platform,
                    key_ref: vec![],
                    blob: vec![],
                },
            })
        });
        assert_eq!(
            Keybag::parse(&b, &Pins::default()).unwrap_err(),
            VaultError::RejectedPlatform(platform)
        );
    }
    let no_pass = signed_with(&|kb| kb.wrappers.retain(|w| w.kind() != 1));
    assert!(matches!(
        Keybag::parse(&no_pass, &Pins::default()),
        Err(VaultError::Malformed(_))
    ));
    let no_seed = signed_with(&|kb| kb.wrappers.retain(|w| w.kind() != KIND_SIGNSEED));
    assert!(matches!(
        Keybag::parse(&no_seed, &Pins::default()),
        Err(VaultError::Malformed(_))
    ));
    let dup = signed_with(&|kb| kb.wrappers.push(kb.wrappers[1].clone()));
    assert!(matches!(
        Keybag::parse(&dup, &Pins::default()),
        Err(VaultError::Malformed(_))
    ));

    let flags = forge(v, |u| u[6..8].copy_from_slice(&3u16.to_be_bytes()));
    assert!(matches!(
        Keybag::parse(&flags, &Pins::default()),
        Err(VaultError::Malformed(_))
    ));
    let version = forge(v, |u| u[4..6].copy_from_slice(&2u16.to_be_bytes()));
    assert_eq!(
        Keybag::parse(&version, &Pins::default()).unwrap_err(),
        VaultError::UnsupportedVersion
    );
    let trailing = forge(v, |u| u.push(0));
    assert!(matches!(
        Keybag::parse(&trailing, &Pins::default()),
        Err(VaultError::Malformed(_))
    ));
    // First wrapper body starts at 111: kdf_id, then m_kib.
    let kdf_id = forge(v, |u| u[111] = 2);
    assert!(matches!(
        Keybag::parse(&kdf_id, &Pins::default()),
        Err(VaultError::Malformed(_))
    ));
    let low_m = forge(v, |u| u[112..116].copy_from_slice(&1u32.to_be_bytes()));
    assert_eq!(
        Keybag::parse(&low_m, &Pins::default()).unwrap_err(),
        VaultError::KdfBounds
    );
    let zero_count = forge(v, |u| u[84..86].copy_from_slice(&0u16.to_be_bytes()));
    assert!(matches!(
        Keybag::parse(&zero_count, &Pins::default()),
        Err(VaultError::Malformed(_))
    ));
}

#[test]
fn every_single_byte_change_and_every_truncation_fails() {
    let nv = new_vault(true);
    let good = nv.vault.keybag_bytes().to_vec();
    assert!(Keybag::parse(&good, &Pins::default()).is_ok());
    for i in 0..good.len() {
        let mut bad = good.clone();
        bad[i] ^= 0x01;
        assert!(
            Keybag::parse(&bad, &Pins::default()).is_err(),
            "byte {i} flip was accepted"
        );
    }
    for n in 0..good.len() {
        assert!(
            Keybag::parse(&good[..n], &Pins::default()).is_err(),
            "truncation to {n} was accepted"
        );
    }
}
