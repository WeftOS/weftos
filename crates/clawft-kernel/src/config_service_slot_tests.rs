//! Card 10a2eb4a: slot names must be unambiguous, and `new(key)` must
//! use the caller's key.
//!
//! Before this card every store was keyed by `format!("{namespace}/{key}")`
//! with no check on either part, so `("a/b", "c")` and `("a", "b/c")` were
//! the same slot. A write through one name replaced the value and the
//! `scoped_to` ACL stored under the other.

use super::*;

fn pid(n: u64) -> Pid {
    n
}

/// Names that must be refused as a namespace or as a key.
const BAD_NAMES: &[&str] = &[
    "a/b", "/a", "a/", "/", "a\\b", "", ".", "..", "a\nb", "a\0b", "a\tb", "a\u{7f}b",
];

#[test]
fn aliased_secret_write_cannot_replace_value_or_acl() {
    let svc = ConfigService::new([0xAB; 32]);
    // Victim: only PID 1 may read it.
    let _ = svc.set_secret("a", "b/c", b"victim", vec![pid(1)]);
    // Attacker writes the aliasing pair with an empty (open) ACL.
    let _ = svc.set_secret("a/b", "c", b"evil", vec![]);
    for (ns, key) in [("a", "b/c"), ("a/b", "c")] {
        if let Ok(v) = svc.get_secret(ns, key, pid(99)) {
            panic!("PID 99 read {ns:?}/{key:?} through an aliased slot: {:?}", &v[..]);
        }
    }
    assert!(svc.secrets.is_empty(), "no secret may be stored under a name with '/'");
    assert!(svc.secret_refs.is_empty());
}

#[test]
fn separator_or_control_char_in_secret_names_is_rejected() {
    let svc = ConfigService::new([0xAB; 32]);
    for bad in BAD_NAMES {
        assert!(
            svc.set_secret(bad, "k", b"v", vec![]).is_err(),
            "namespace {bad:?} must be rejected"
        );
        assert!(
            svc.set_secret("ns", bad, b"v", vec![]).is_err(),
            "key {bad:?} must be rejected"
        );
        assert!(svc.get_secret(bad, "k", pid(1)).is_err());
        assert!(svc.get_secret("ns", bad, pid(1)).is_err());
    }
    assert!(svc.secrets.is_empty());
}

#[test]
fn aliased_config_write_cannot_overwrite_or_leak_into_another_namespace() {
    let svc = ConfigService::new_default();
    svc.set("a", "x", serde_json::json!(1), pid(1)).unwrap();
    let _ = svc.set("a", "b/c", serde_json::json!("victim"), pid(1));
    let _ = svc.set("a/b", "c", serde_json::json!("evil"), pid(2));
    let _ = svc.set_typed("a/b", "d", ConfigValue::Integer(7), pid(2));
    assert_ne!(svc.get("a", "b/c"), Some(serde_json::json!("evil")));
    assert!(svc.get("a", "b/c").is_none());
    assert!(svc.get_typed("a", "b/d").is_none());
    assert_eq!(svc.list_keys("a"), vec!["x".to_string()]);
    assert!(svc.list("a").is_empty());
    assert!(svc.delete("a/b", "c", pid(2)).is_err());
    assert!(svc.delete_typed("a", "b/d", pid(2)).is_err());
    assert_eq!(svc.get("a", "x"), Some(serde_json::json!(1)));
}

#[test]
fn separator_or_control_char_in_config_names_is_rejected() {
    let svc = ConfigService::new_default();
    for bad in BAD_NAMES {
        assert!(svc.set(bad, "k", serde_json::json!(1), pid(1)).is_err(), "{bad:?}");
        assert!(svc.set("ns", bad, serde_json::json!(1), pid(1)).is_err(), "{bad:?}");
        assert!(svc.set_typed(bad, "k", ConfigValue::Boolean(true), pid(1)).is_err());
        assert!(svc.set_typed("ns", bad, ConfigValue::Boolean(true), pid(1)).is_err());
        assert!(svc.delete(bad, "k", pid(1)).is_err());
        assert!(svc.delete_typed("ns", bad, pid(1)).is_err());
        assert!(svc.get(bad, "k").is_none());
        assert!(svc.get_typed("ns", bad).is_none());
        assert!(svc.list_keys(bad).is_empty());
        assert!(svc.list(bad).is_empty());
    }
    assert!(svc.configs.is_empty() && svc.entries.is_empty());
    assert!(svc.change_log().is_empty(), "a rejected write must not be logged");
}

#[test]
fn ordinary_names_still_work() {
    let svc = ConfigService::new([0xAB; 32]);
    for (ns, key) in [("creds", "api_key"), ("app.v2", "my-key:1"), ("ns", "a.b"), ("ns", "...x")] {
        svc.set(ns, key, serde_json::json!(true), pid(1)).unwrap();
        assert_eq!(svc.get(ns, key), Some(serde_json::json!(true)));
        svc.set_secret(ns, key, b"value", vec![pid(1)]).unwrap();
        assert_eq!(&svc.get_secret(ns, key, pid(1)).unwrap()[..], b"value");
    }
}

// ── item 3: new(key) uses the caller's key ─────────────────────────────

#[test]
fn same_key_opens_across_instances() {
    let a = ConfigService::new([0x5A; 32]);
    let b = ConfigService::new([0x5A; 32]);
    a.set_secret("creds", "k", b"shared-key-value", vec![]).unwrap();
    b.set_secret("creds", "k", b"placeholder", vec![]).unwrap();
    let sealed = a.secrets.get("creds/k").unwrap().clone();
    b.secrets.insert("creds/k".into(), sealed);
    assert_eq!(
        &b.get_secret("creds", "k", pid(1)).unwrap()[..],
        b"shared-key-value",
        "two services built with the same key must open each other's values"
    );
}
