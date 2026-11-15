//! Sealed index tests. All values are fake.

use crate::VaultError;
use crate::blob::{Policy, SecretType};
use crate::index::IndexEntry;
use crate::keybag::Pins;
use crate::tests::{PASS, new_vault};
use crate::vault::Vault;

fn entry(name: &str, ty: SecretType, tags: &[&str], source: Option<&str>) -> IndexEntry {
    IndexEntry {
        name: name.into(),
        secret_type: ty,
        policy: if ty == SecretType::Token {
            Policy::Standing
        } else {
            Policy::PerUse
        },
        revision: 3,
        last_used_unix: 1_800_000_123,
        tags: tags.iter().map(|t| t.to_string()).collect(),
        source: source.map(str::to_string),
    }
}

fn sample() -> Vec<IndexEntry> {
    vec![
        entry(
            "login/work-mail-FAKE",
            SecretType::Login,
            &["mail", "work-FAKE-tag"],
            None,
        ),
        entry(
            "token/ci-FAKE",
            SecretType::Token,
            &[],
            Some("import-FAKE-source"),
        ),
        entry("pay/card-FAKE", SecretType::Payment, &["card"], None),
    ]
}

fn by_name(mut v: Vec<IndexEntry>) -> Vec<IndexEntry> {
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn index_round_trip() {
    let v = new_vault(true).vault;
    let s = v.seal_index_entries(&sample(), 5).unwrap();
    assert_eq!(s.slot, "index");
    let got = v.open_index(&s.bytes).unwrap();
    assert_eq!(by_name(got), by_name(sample()));

    // Order of input does not matter: the payload is canonical.
    let mut rev = sample();
    rev.reverse();
    assert_eq!(
        v.encode_index(&sample()).unwrap(),
        v.encode_index(&rev).unwrap()
    );

    // Empty index, and a fresh unlock opens it.
    let e = v.seal_index_entries(&[], 1).unwrap();
    assert!(v.open_index(&e.bytes).unwrap().is_empty());
    let v2 = Vault::unlock(v.keybag_bytes(), PASS, &mut Pins::default()).unwrap();
    assert_eq!(v2.open_index(&s.bytes).unwrap().len(), 3);
}

#[test]
fn a_tampered_index_is_refused() {
    let v = new_vault(true).vault;
    let s = v.seal_index_entries(&sample(), 5).unwrap();
    for i in 0..s.bytes.len() {
        let mut bad = s.bytes.clone();
        bad[i] ^= 1;
        assert!(v.open_index(&bad).is_err(), "byte {i} flip accepted");
    }
    assert!(v.open_index(&s.bytes[..s.bytes.len() - 1]).is_err());
    let mut longer = s.bytes.clone();
    longer.push(0);
    assert!(v.open_index(&longer).is_err());
    // Another vault, or the blob moved to a secret slot, refuses it.
    assert!(new_vault(true).vault.open_index(&s.bytes).is_err());
    assert_eq!(
        v.open_secret(&format!("s/{}", "0".repeat(64)), &s.bytes)
            .unwrap_err(),
        VaultError::AuthFailed
    );
}

#[test]
fn a_validly_sealed_but_non_canonical_payload_is_refused() {
    let v = new_vault(true).vault;
    let a = entry("login/a-FAKE", SecretType::Login, &[], None);
    let b = entry("login/b-FAKE", SecretType::Login, &[], None);
    let pa = v.encode_index(std::slice::from_ref(&a)).unwrap();
    let pb = v.encode_index(std::slice::from_ref(&b)).unwrap();
    let (ea, eb) = (&pa[10..], &pb[10..]);
    let mk = |count: u32, parts: &[&[u8]]| {
        let mut p = pa[..6].to_vec();
        p.extend(count.to_be_bytes());
        parts.iter().for_each(|x| p.extend(*x));
        p
    };
    let open = |p: Vec<u8>| v.open_index(&v.seal_index(&p, 1).unwrap().bytes);
    // Sanity: the sorted pair opens.
    let (lo, hi) = if ea[..32] < eb[..32] {
        (ea, eb)
    } else {
        (eb, ea)
    };
    assert_eq!(open(mk(2, &[lo, hi])).unwrap().len(), 2);
    // Descending order, a repeated entry, trailing bytes, a short count, a wrong version: all refused.
    assert!(matches!(
        open(mk(2, &[hi, lo])),
        Err(VaultError::Malformed(_))
    ));
    assert!(matches!(
        open(mk(2, &[ea, ea])),
        Err(VaultError::Malformed(_))
    ));
    assert!(matches!(
        open(mk(1, &[lo, hi])),
        Err(VaultError::Malformed(_))
    ));
    assert!(matches!(
        open(mk(1, &[lo, &[0]])),
        Err(VaultError::Malformed(_))
    ));
    assert!(matches!(
        open(mk(3, &[lo, hi])),
        Err(VaultError::Malformed(_))
    ));
    let mut ver = mk(1, &[ea]);
    ver[1] = 2;
    assert_eq!(open(ver).unwrap_err(), VaultError::UnsupportedVersion);
    // A name that does not match its hash (one letter changed in place).
    let mut renamed = mk(1, &[ea]);
    let at = renamed.windows(5).position(|w| w == b"login").unwrap();
    renamed[at] = b'L';
    assert!(matches!(open(renamed), Err(VaultError::Malformed(_))));
    // A blob of another type at the index slot.
    let raw = v.seal_secret(&crate::tests::req("x", b"y")).unwrap();
    let key = crate::hkdf_keys::blob_key(&v.keybag().vault_id, &v.vk, 1);
    let p = v.seal_params(&key, "index", 1).unwrap();
    let as_login = crate::blob::seal_blob(&p, SecretType::Login, Policy::PerUse, &pa).unwrap();
    assert!(matches!(
        v.open_index(&as_login),
        Err(VaultError::Malformed(_))
    ));
    drop(raw);
}

#[test]
fn input_rules_are_enforced() {
    let v = new_vault(true).vault;
    let dup = [
        entry("a", SecretType::Login, &[], None),
        entry("a", SecretType::Token, &[], None),
    ];
    assert!(v.seal_index_entries(&dup, 1).is_err());
    assert!(
        v.seal_index_entries(&[entry("a", SecretType::Index, &[], None)], 1)
            .is_err()
    );
    assert!(
        v.seal_index_entries(&[entry("", SecretType::Login, &[], None)], 1)
            .is_err()
    );
    let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();
    assert!(
        v.seal_index_entries(&[entry("a", SecretType::Login, &refs, None)], 1)
            .is_err()
    );
    // Not NFC: "e" + combining acute.
    assert!(
        v.seal_index_entries(&[entry("e\u{301}", SecretType::Login, &[], None)], 1)
            .is_err()
    );
    assert!(
        v.seal_index_entries(&[entry("\u{e9}", SecretType::Login, &[], None)], 1)
            .is_ok()
    );
}

#[test]
fn no_plaintext_names_in_the_stored_bytes() {
    let v = new_vault(true).vault;
    let s = v.seal_index_entries(&sample(), 5).unwrap();
    let needles: Vec<&[u8]> = vec![
        b"login/work-mail-FAKE",
        b"token/ci-FAKE",
        b"pay/card-FAKE",
        b"work-FAKE-tag",
        b"import-FAKE-source",
        b"FAKE",
        b"login",
        b"mail",
    ];
    // The control: the same grep finds every name in the plaintext payload, so it can fail.
    let plain = v.encode_index(&sample()).unwrap();
    for n in &needles {
        assert!(
            contains(&plain, n),
            "control missed {:?}",
            String::from_utf8_lossy(n)
        );
        assert!(
            !contains(&s.bytes, n),
            "stored index leaks {:?}",
            String::from_utf8_lossy(n)
        );
    }
    // The secret slot names are keyed hashes: no name appears in them either.
    let slot = v.slot_for_name("login/work-mail-FAKE").unwrap();
    assert!(slot.starts_with("s/") && !slot.contains("login") && !slot.contains("FAKE"));
}

#[test]
fn an_index_sealed_before_a_rotation_still_opens() {
    let mut v = new_vault(true).vault;
    let old = v.seal_index_entries(&sample(), 1).unwrap();
    v.rotate_vk_unchecked(PASS, None, &mut Pins::default())
        .unwrap();
    assert_eq!(
        by_name(v.open_index(&old.bytes).unwrap()),
        by_name(sample())
    );
    // Re-sealing under the new generation uses the new name keys.
    let fresh = v.seal_index_entries(&sample(), 2).unwrap();
    assert_eq!(
        by_name(v.open_index(&fresh.bytes).unwrap()),
        by_name(sample())
    );
    assert_ne!(v.encode_index(&sample()).unwrap()[2..6], old.bytes[6..10]);
}
