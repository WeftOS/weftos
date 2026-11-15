//! Secret, blob, wrapper-op, recovery-code, Debug and log-capture tests. All values are fake.

use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::blob::{self, Policy, SealParams, SecretType};
use crate::keybag::{KIND_SIGNSEED, Keybag, Pins, WrapperBody};
use crate::tests::{PASS, T, hex, new_vault, pass_id, req};
use crate::vault::Vault;
use crate::{RecoveryKey, SecretSeal, VaultError, hkdf_keys};

#[test]
fn secret_roundtrip_moved_blob_and_tamper() {
    let nv = new_vault(true);
    let v = &nv.vault;
    let s = v
        .seal_secret(&req("login/work-mail", b"FAKE-secret-value-0001"))
        .unwrap();
    assert!(s.slot.starts_with("s/") && s.slot.len() == 66);
    let ok = v.open_named("login/work-mail", &s.bytes).unwrap();
    assert_eq!(&ok.payload[..], b"FAKE-secret-value-0001");
    assert_eq!(
        (ok.secret_type, ok.policy, ok.revision),
        (SecretType::Login, Policy::PerUse, 1)
    );

    // A moved blob fails: another name's slot, the index slot, a zero slot.
    let other = v.slot_for_name("login/other").unwrap();
    for slot in [other.as_str(), "index", &format!("s/{}", "0".repeat(64))] {
        assert_eq!(
            v.open_secret(slot, &s.bytes).unwrap_err(),
            VaultError::AuthFailed
        );
    }
    // Another vault cannot open it, even at the same slot.
    let nv2 = new_vault(true);
    assert!(nv2.vault.open_secret(&s.slot, &s.bytes).is_err());
    // Every byte change fails.
    for i in 0..s.bytes.len() {
        let mut bad = s.bytes.clone();
        bad[i] ^= 0x01;
        assert!(
            v.open_secret(&s.slot, &bad).is_err(),
            "blob byte {i} flip was accepted"
        );
    }
    // Name normalization: NFC and NFD forms share a slot.
    assert_eq!(
        v.slot_for_name("caf\u{e9}").unwrap(),
        v.slot_for_name("cafe\u{301}").unwrap()
    );
    // Case-sensitive.
    assert_ne!(
        v.slot_for_name("Login").unwrap(),
        v.slot_for_name("login").unwrap()
    );
}

#[test]
fn blob_limits_padding_and_index() {
    let nv = new_vault(true);
    let v = &nv.vault;
    for (len, total) in [
        (0usize, 42 + 256 + 16),
        (250, 42 + 256 + 16),
        (251, 42 + 512 + 16),
        (65530, 42 + 65536 + 16),
    ] {
        let s = v.seal_secret(&req("n", &vec![7u8; len])).unwrap();
        assert_eq!(s.bytes.len(), total, "payload {len}");
        assert_eq!(v.open_named("n", &s.bytes).unwrap().payload.len(), len);
    }
    assert_eq!(
        v.seal_secret(&req("n", &vec![7u8; 65531])).unwrap_err(),
        VaultError::TooLarge("payload")
    );
    assert_eq!(
        v.seal_secret(&req("", b"x")).unwrap_err(),
        VaultError::TooLarge("name")
    );
    assert_eq!(
        v.seal_secret(&req(&"x".repeat(256), b"x")).unwrap_err(),
        VaultError::TooLarge("name")
    );
    let mut r = req("n", b"x");
    r.revision = 0;
    assert!(v.seal_secret(&r).is_err());
    r.revision = 1;
    r.secret_type = SecretType::Index;
    assert!(v.seal_secret(&r).is_err());

    let idx = v.seal_index(b"{\"items\":[]}", 3).unwrap();
    assert_eq!(idx.slot, "index");
    let o = v.open_secret("index", &idx.bytes).unwrap();
    assert_eq!((o.secret_type, o.revision), (SecretType::Index, 3));

    // Nonzero padding is rejected even when the AEAD tag is valid.
    let bk = hkdf_keys::blob_key(&v.keybag.vault_id, &v.vk, 1);
    let p = SealParams {
        vault_id: &v.keybag.vault_id,
        blob_key: &bk,
        generation: 1,
        slot: "index",
        revision: 1,
        nonce: [5; 24],
    };
    let mut pt = vec![0u8; 256];
    pt[0] = 16;
    pt[5] = 1; // payload_len = 1
    pt[200] = 9; // nonzero padding
    let bad = blob::seal_plaintext(&p, &pt).unwrap();
    assert!(matches!(
        v.open_secret("index", &bad),
        Err(VaultError::Malformed(_))
    ));
    // Unknown policy byte.
    let mut pt2 = vec![0u8; 256];
    pt2[0] = 1;
    pt2[1] = 2;
    let bad2 = blob::seal_plaintext(&p, &pt2).unwrap();
    assert!(matches!(
        v.open_secret("index", &bad2),
        Err(VaultError::Malformed(_))
    ));
}

#[test]
fn draft_vault_refuses_real_credentials() {
    let draft = new_vault(true);
    let mut r = req("login/x", b"FAKE");
    r.real_credential = true;
    assert_eq!(
        draft.vault.seal_secret(&r).unwrap_err(),
        VaultError::DraftRefusesRealCredentials
    );
    let reviewed = new_vault(false);
    assert!(!reviewed.vault.is_draft());
    assert!(reviewed.vault.seal_secret(&r).is_ok());
}

#[test]
fn wrapper_add_remove_and_recovery_unlock() {
    let mut nv = new_vault(true);
    let mut pins = Pins::default();
    let rk = nv.vault.add_recovery_wrapper().unwrap();
    assert_eq!(nv.vault.keybag().sequence, 2);
    let bytes = nv.vault.keybag_bytes().to_vec();
    assert!(Vault::unlock_with_recovery(&bytes, &rk, &mut pins).is_ok());
    // The original recovery key (from create) opens the original wrapper only if still present.
    assert!(Vault::unlock_with_recovery(&bytes, &nv.recovery_key, &mut Pins::default()).is_ok());

    let id = nv
        .vault
        .keybag()
        .wrappers
        .iter()
        .rev()
        .find(|w| w.kind() == 3)
        .unwrap()
        .id;
    let after = nv.vault.remove_wrapper(&id).unwrap().to_vec();
    assert_eq!(nv.vault.keybag().sequence, 3);
    assert_eq!(
        Vault::unlock_with_recovery(&after, &rk, &mut Pins::default()).unwrap_err(),
        VaultError::UnlockFailed
    );

    // Device wrapper: the closure gets the id, AAD and VK, and the platform blob is stored opaque.
    let dev = nv
        .vault
        .add_device_wrapper(1, b"fake-key-ref", |id, aad, vk| {
            assert_eq!(aad.len(), 13 + 16 + 16 + 1 + 4);
            assert_eq!(id.len(), 16);
            let mut blob = b"FAKE-DEVICE-BLOB".to_vec();
            blob.push(vk[0]);
            Ok(blob)
        })
        .unwrap();
    let reparsed = Keybag::parse(nv.vault.keybag_bytes(), &Pins::default()).unwrap();
    assert!(matches!(
        reparsed.wrapper(&dev).unwrap().body,
        WrapperBody::Device { platform: 1, .. }
    ));
    assert_eq!(
        nv.vault
            .add_device_wrapper(3, b"", |_, _, _| Ok(vec![]))
            .unwrap_err(),
        VaultError::RejectedPlatform(3)
    );
    nv.vault.remove_wrapper(&dev).unwrap();

    // The signing seed wrapper and the last passcode wrapper cannot be removed.
    let seed = nv
        .vault
        .keybag()
        .wrappers
        .iter()
        .find(|w| w.kind() == KIND_SIGNSEED)
        .unwrap()
        .id;
    assert!(nv.vault.remove_wrapper(&seed).is_err());
    let pid = pass_id(&nv.vault);
    assert!(matches!(
        nv.vault.remove_wrapper(&pid),
        Err(VaultError::Malformed(_))
    ));
    assert!(nv.vault.remove_wrapper(&[0xee; 16]).is_err());
    // A failed change leaves the keybag as it was.
    assert_eq!(nv.vault.keybag().sequence, 5);
}

#[test]
fn recovery_code_roundtrip_and_typo_detection() {
    let nv = new_vault(true);
    let code = nv.recovery_key.to_code();
    assert!(code.starts_with("WVR1-"));
    assert_eq!(code.len(), 5 + 55 + 13); // 13 separators between 14 groups
    let back = RecoveryKey::from_code(&code).unwrap();
    assert_eq!(&back.0[..], &nv.recovery_key.0[..]);
    assert!(RecoveryKey::from_code(&code.to_lowercase()).is_ok());
    // Flip one character: the checksum or alphabet check rejects it.
    let mut chars: Vec<char> = code.chars().collect();
    let i = 8;
    chars[i] = if chars[i] == '2' { '3' } else { '2' };
    let typo: String = chars.into_iter().collect();
    assert_eq!(
        RecoveryKey::from_code(&typo).unwrap_err(),
        VaultError::BadRecoveryCode
    );
    assert!(RecoveryKey::from_code("WVR1-ABCD").is_err());
    assert!(RecoveryKey::from_code("XXXX-ABCD").is_err());
}

#[test]
fn secret_seal_wire_format() {
    let s = SecretSeal::from_key(&[7u8; 32]);
    let sealed = s.seal(b"ns/key", b"FAKE-value").unwrap();
    assert_eq!(sealed.len(), 24 + 10 + 16);
    assert_eq!(&s.open(b"ns/key", &sealed).unwrap()[..], b"FAKE-value");
    assert_eq!(
        s.open(b"ns/other", &sealed).unwrap_err(),
        VaultError::AuthFailed
    );
    assert!(s.open(b"ns/key", &sealed[..39]).is_err());
    let mut bad = sealed.clone();
    bad[30] ^= 1;
    assert!(s.open(b"ns/key", &bad).is_err());
    assert_ne!(
        sealed,
        s.seal(b"ns/key", b"FAKE-value").unwrap(),
        "fresh nonce per seal"
    );
    assert_eq!(format!("{s:?}"), "SecretSeal(<redacted>)");
}

#[test]
fn debug_and_display_never_show_secrets() {
    let nv = new_vault(true);
    let r = req("login/fake-name-77", b"FAKE-DEBUG-PAYLOAD-77");
    let sealed = nv.vault.seal_secret(&r).unwrap();
    let opened = nv
        .vault
        .open_named("login/fake-name-77", &sealed.bytes)
        .unwrap();
    let code = nv.recovery_key.to_code();
    let mut shown = vec![
        format!("{nv:?}"),
        format!("{:?}", nv.vault),
        format!("{:?}", nv.recovery_key),
        format!("{opened:?}"),
        format!("{r:?}"),
        format!("{:?}", SecretSeal::from_key(&[1; 32])),
    ];
    for e in [
        VaultError::AuthFailed,
        VaultError::UnlockFailed,
        VaultError::BadRecoveryCode,
        VaultError::DraftRefusesRealCredentials,
        VaultError::Rollback { got: 1, seen: 2 },
    ] {
        shown.push(format!("{e} {e:?}"));
    }
    let all = shown.join("\n");
    let (vk_h, rk_h, sk_h) = (
        hex(&nv.vault.vk[..]),
        hex(&nv.recovery_key.0[..]),
        hex(&nv.vault.signing.to_bytes()),
    );
    let secrets: [&str; 7] = [
        "FAKE-DEBUG-PAYLOAD-77",
        "fake-name-77",
        PASS,
        &code,
        &vk_h,
        &rk_h,
        &sk_h,
    ];
    for s in secrets {
        assert!(
            !all.contains(s),
            "debug or display output contains a secret"
        );
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

/// One process-wide subscriber: a scoped `with_default` loses events to callsite-interest caching
/// when other test threads reach the same callsites first.
fn global_capture() -> &'static Capture {
    static CAP: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
    CAP.get_or_init(|| {
        let cap = Capture::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(cap.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(sub)
            .expect("no other global subscriber in this test binary");
        cap
    })
}

#[test]
fn no_secret_in_captured_log_output() {
    let cap = global_capture();
    let mut nv = Vault::create_unchecked("FAKE-LOG-PASSCODE-xyz", T, true).unwrap();
    let sealed = nv
        .vault
        .seal_secret(&req("login/fake-log-name", b"FAKE-LOG-SECRET-VALUE"))
        .unwrap();
    let _ = nv
        .vault
        .open_named("login/fake-log-name", &sealed.bytes)
        .unwrap();
    let _ = nv.vault.open_secret("index", &sealed.bytes);
    let id = pass_id(&nv.vault);
    nv.vault
        .change_passcode_unchecked(&id, "FAKE-LOG-NEW-PASSCODE", T)
        .unwrap();
    let _ = Vault::unlock(
        nv.vault.keybag_bytes(),
        "FAKE-LOG-WRONG-PASSCODE",
        &mut Pins::default(),
    );
    let _ = Vault::unlock(
        nv.vault.keybag_bytes(),
        "FAKE-LOG-NEW-PASSCODE",
        &mut Pins::default(),
    )
    .unwrap();
    let _ = Vault::unlock_with_recovery(
        nv.vault.keybag_bytes(),
        &nv.recovery_key,
        &mut Pins::default(),
    )
    .unwrap();
    let (code, vk_hex, rk_hex, seed_hex) = (
        nv.recovery_key.to_code().to_string(),
        hex(&nv.vault.vk[..]),
        hex(&nv.recovery_key.0[..]),
        hex(&nv.vault.signing.to_bytes()),
    );

    let out = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
    for event in [
        "vault created",
        "secret sealed",
        "keybag rewritten",
        "vault unlocked",
        "unlock failed",
    ] {
        assert!(out.contains(event), "capture saw no {event:?} event");
    }
    for s in [
        "FAKE-LOG-PASSCODE-xyz",
        "FAKE-LOG-NEW-PASSCODE",
        "FAKE-LOG-WRONG-PASSCODE",
        "FAKE-LOG-SECRET-VALUE",
        "fake-log-name",
        &code,
        &vk_hex,
        &rk_hex,
        &seed_hex,
    ] {
        assert!(!out.contains(s), "log output contains a secret");
    }
}
