//! The spec's published test vectors (section 10, all FAKE) must reproduce byte for byte.

use sha2::{Digest, Sha256};

use crate::blob::{self, Policy, SealParams, SecretType};
use crate::hkdf_keys;
use crate::keybag::{Keybag, Pins};
use crate::passcode_kdf::{KdfParams, derive_kek};
use crate::tests::{hex, unhex};
use crate::vault::{CreateInputs, Vault};
use crate::{VaultError, normalize};

const VAULT_ID: &[u8; 16] = b"FAKE-VAULT-ID-01";
const PASSCODE: &str = "FAKE-PASSCODE-not-a-real-secret-0001";

fn seq<const N: usize>(start: u8) -> [u8; N] {
    core::array::from_fn(|i| start + i as u8)
}

fn inputs() -> CreateInputs {
    CreateInputs {
        vault_id: *VAULT_ID,
        vk: seq(0x00),
        signing_seed: seq(0x80),
        salt: [0x11; 16],
        recovery: seq(0x40),
        wrapper_ids: [seq(0xa0), seq(0xb0), seq(0xc0)],
        nonces: [seq(0x30), seq(0x50), seq(0x60)],
        now: 1_800_000_000,
        draft: true,
        params: KdfParams::RECOMMENDED,
    }
}

const KEYBAG_HEX: &str = "57564b420001000146414b452d5641554c542d49442d30310000000000000001000000006b49d200000000006b49d20000000001cd14b37f956e953194ff7fb73b3d81dcc561d61a7538094b7c3e1a643ee5f3aa000301a0a1a2a3a4a5a6a7a8a9aaabacadaeaf00000001000000650100010000000000030000000111111111111111111111111111111111303132333435363738393a3b3c3d3e3f40414243444546478b2d98791a0cbafa3f844a57e11530d2f19a1c36b68a49d9641732e64ffcd8db80a89b20132c3a15dee503b9e7e99e4503b0b1b2b3b4b5b6b7b8b9babbbcbdbebf0000000100000048505152535455565758595a5b5c5d5e5f6061626364656667940a244640d856013da103dee939cfe8a9f665dc15c84c8e07453deeaa7f524c1e6292e4233846771f855ff4876d942005c0c1c2c3c4c5c6c7c8c9cacbcccdcecf0000000100000048606162636465666768696a6b6c6d6e6f707172737475767720fec19f01980701185660c20501fe5e4a29eac7c9883fe1e2ac8e0c3f2194582e63940b9c8972897326b355f90d9c3461178251494024b3accc36508170995b89114e495420eef9bbfe10c5da724710ceaba0eaf95da07d750d91dce58eb733611f6ee2a414647cc0631a31b37c5105";

const BLOB_HEX: &str = "5756424c0101000000010000000000000007707172737475767778797a7b7c7d7e7f8081828384858687e6955eb96766047ab2fc31b34c83244881b91460965569cc8d6d487304788c7ab0dea17c9ac3d976ae4526e7a0b8a67ff7738271cf06842471fcd39cfe9c2b5818f159b70ec9b288f04ff2b7d3c37595c22327ed669a57545cbb1e5d4874b87eca27f0686e848f42703a0a03ee355abfc52cea2773c17dd155744af44515a4586b91961be80bfdfda222b7fc32ef9838353b22e00cf492164071c88ff22508ff83e3a8edec78ff8f35a7312bef2378bbb2ce4d4003329beec0d2b4ffb044b7844808a87d593ca57834c9caae5e90cb3cffcee9c06b0ae40b91035dcf86191d21b7bf19f6267b8705e0bcf06ceb98352132ea9e2f7756d16dfaaf76a939a2390328c4346ea624f2d41615af9e104448b6";

#[test]
fn derived_keys_match() {
    let vk: [u8; 32] = seq(0);
    assert_eq!(
        hex(&*hkdf_keys::blob_key(VAULT_ID, &vk, 1)),
        "6ac2517ad87b0913425186239e6a1fa6c6b5bc8be8ff3b8f13f9bea01f873a55"
    );
    assert_eq!(
        hex(&*hkdf_keys::name_key(VAULT_ID, &vk, 1)),
        "cf45759f57b16e914feb080dcedeed3ba034bad07686605ef8f1147d956a9509"
    );
    assert_eq!(
        hex(&*hkdf_keys::signseed_key(VAULT_ID, &vk)),
        "149a9e04444c02dabfcac6e2c4ee0d267a070f0b63adb60e469070ef3a5c5ddc"
    );
    let rec: [u8; 32] = seq(0x40);
    assert_eq!(
        hex(&*hkdf_keys::recovery_kek(VAULT_ID, &rec)),
        "6b14c27caf9da5da55e946239082f2b9c22475eb46071e2cef6fd527b716a7e9"
    );
}

#[test]
fn test_profile_kek_and_nfkc() {
    let salt = [0x11u8; 16];
    let kek = derive_kek(&normalize::passcode(PASSCODE), &salt, KdfParams::TEST).unwrap();
    assert_eq!(
        hex(&*kek),
        "41f45d17a3d63a56be476b4551485a34796ae3e5a787998fc9990df4800a07c9"
    );
    // The fullwidth form of the passcode derives the same key.
    let full: String = PASSCODE
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() {
                char::from_u32(c as u32 + 0xFEE0).unwrap()
            } else {
                c
            }
        })
        .collect();
    assert_ne!(full, PASSCODE);
    let kek2 = derive_kek(&normalize::passcode(&full), &salt, KdfParams::TEST).unwrap();
    assert_eq!(hex(&*kek2), hex(&*kek));
}

/// One full-cost (64 MiB, t=3) Argon2id run builds the keybag; the same keybag is then reused for
/// the wrong-passcode and tamper negatives so only two full-cost runs happen in this test.
#[test]
fn keybag_vector_reproduces_and_unlocks() {
    let kek = derive_kek(
        &normalize::passcode(PASSCODE),
        &[0x11; 16],
        KdfParams::RECOMMENDED,
    )
    .unwrap();
    assert_eq!(
        hex(&*kek),
        "da0676c8ecbcdda2ad3a0ab207ab5ec5de2e41b5fe31be09f6818406f5b52bb1"
    );

    let nv = Vault::create_with(PASSCODE, inputs()).unwrap();
    let want = unhex(KEYBAG_HEX);
    assert_eq!(want.len(), 470);
    assert_eq!(
        hex(&Sha256::digest(&want)),
        "199b96dfd5d554b47715c5c1759f48871719715c5332c32a39f5db29d752675e"
    );
    assert_eq!(nv.vault.keybag_bytes(), want.as_slice());
    assert_eq!(
        hex(&nv.vault.keybag().sign_pub),
        "cd14b37f956e953194ff7fb73b3d81dcc561d61a7538094b7c3e1a643ee5f3aa"
    );
    assert!(nv.vault.is_draft());

    // Unlock the published keybag by passcode (second full-cost run).
    let mut pins = Pins::default();
    let v = Vault::unlock(&want, PASSCODE, &mut pins).unwrap();
    assert_eq!(v.keybag_bytes(), want.as_slice());
    assert_eq!(pins.highest.map(|h| h.sequence), Some(1));

    // Negative 4: one byte of sequence flipped, signature verification fails. (Flipping the low
    // byte would make the sequence 0, which is rejected structurally before the signature check.)
    let mut bad = want.clone();
    bad[24] ^= 1;
    assert_eq!(
        Keybag::parse(&bad, &Pins::default()).unwrap_err(),
        VaultError::BadSignature
    );

    // Recovery key unlocks the same keybag without Argon2id.
    let rk = nv.recovery_key;
    assert!(Vault::unlock_with_recovery(&want, &rk, &mut Pins::default()).is_ok());
}

#[test]
fn wrong_passcode_vector_fails_at_the_tag() {
    // Negative 3 with the full-cost wrapper from the published keybag.
    let want = unhex(KEYBAG_HEX);
    let r = Vault::unlock(
        &want,
        "FAKE-PASSCODE-not-a-real-secret-0002",
        &mut Pins::default(),
    );
    assert_eq!(r.unwrap_err(), VaultError::UnlockFailed);
}

#[test]
fn blob_vector_reproduces_and_negatives_fail() {
    let vk: [u8; 32] = seq(0);
    let nk = hkdf_keys::name_key(VAULT_ID, &vk, 1);
    let bk = hkdf_keys::blob_key(VAULT_ID, &vk, 1);
    let hash = blob::name_hash_hex(&nk, "login/work-mail").unwrap();
    assert_eq!(
        hash,
        "cf817b908cdf1a000539b65e7f2d7333d47d24dd85d90d9a710511630fcf2934"
    );
    let slot = format!("s/{hash}");
    assert_eq!(
        hex(&blob::blob_aad(VAULT_ID, &slot, 1, 7)),
        "57564c542d424c4f422d76310046414b452d5641554c542d49442d303142732f636638313762393038636466316130303035333962363565376632643733333364343764323464643835643930643961373130353131363330666366323933340101000000010000000000000007"
    );
    let p = SealParams {
        vault_id: VAULT_ID,
        blob_key: &bk,
        generation: 1,
        slot: &slot,
        revision: 7,
        nonce: seq(0x70),
    };
    let sealed = blob::seal_blob(
        &p,
        SecretType::Login,
        Policy::PerUse,
        b"FAKE-secret-value-0001",
    )
    .unwrap();
    assert_eq!(sealed, unhex(BLOB_HEX));
    assert_eq!(sealed.len(), 314);
    assert_eq!(
        hex(&Sha256::digest(&sealed)),
        "b73bfc8efea9b4c9a060d50d4e126a5410f4a4fe5cc4db9f82842ed692966a4c"
    );

    let ok = blob::open_blob(VAULT_ID, &bk, 1, &slot, &sealed).unwrap();
    assert_eq!(&ok.payload[..], b"FAKE-secret-value-0001");
    assert_eq!(
        (ok.secret_type, ok.policy, ok.revision),
        (SecretType::Login, Policy::PerUse, 7)
    );

    // Negative 1: revision changed in the AAD (header byte flipped).
    let mut rev = sealed.clone();
    rev[17] ^= 1;
    assert_eq!(
        blob::open_blob(VAULT_ID, &bk, 1, &slot, &rev).unwrap_err(),
        VaultError::AuthFailed
    );
    // Negative 2: opened as slot s/ + 64 zeros.
    let zero_slot = format!("s/{}", "0".repeat(64));
    assert_eq!(
        blob::open_blob(VAULT_ID, &bk, 1, &zero_slot, &sealed).unwrap_err(),
        VaultError::AuthFailed
    );
}
