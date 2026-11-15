use serde_json::json;

use super::*;
use std::path::Path;

// ---- S2 tests carried over from feat/adr126-s2 (registry signing, switch, retry, preview, quota, gc) ----

/// S3 review S2 #2: plain `verify` accepts a forged signature under a small-order key.
#[test]
fn a_small_order_trusted_key_is_refused_at_load_and_at_verify_and_strict_checks_are_used() {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    // the identity point: with it, R = identity, S = 0 "verifies" any message under plain verify
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut forged = [0u8; 64];
    forged[0] = 1;
    let vk = VerifyingKey::from_bytes(&identity).unwrap();
    assert!(vk.is_weak());
    assert!(vk.verify(b"any entry bytes", &Signature::from_bytes(&forged)).is_ok(), "the hole: plain verify accepts it");
    // ... so it must be refused wherever we decide who to trust
    let t = Trusted(&[identity]);
    assert!(!t.verify(&hex::encode(identity), "ed25519", b"any entry bytes", &forged), "verifier refuses");
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("trusted");
    std::fs::write(&f, format!("{}\n", hex::encode(identity))).unwrap();
    let path = f.to_string_lossy().into_owned();
    let cfg = SignCfg::from_lookup(|v| (v == "WEFTOS_WT_REGISTRY_TRUSTED").then(|| path.clone()));
    assert!(cfg.err().unwrap().contains("small-order"), "the trusted file is refused at load");
    // a real key still works, and one bit flipped in a good signature does not
    let sk = ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]);
    let pubk = sk.verifying_key().to_bytes();
    let sig = Ed(sk).sign(b"entry");
    let good = Trusted(&[pubk]);
    assert!(good.verify(&hex::encode(pubk), "ed25519", b"entry", &sig));
    let mut bad = sig.clone();
    bad[10] ^= 1;
    assert!(!good.verify(&hex::encode(pubk), "ed25519", b"entry", &bad));
    assert!(!good.verify(&hex::encode(pubk), "ed25519", b"other", &sig));
    // a seed typo does not echo any of what was typed
    let e = hex32("zz-secret-ish").unwrap_err();
    assert!(!e.contains("zz") && !e.contains("secret"), "{e}");
}

/// S3 review S2 #4: a trusted-key file filters, and the verbs say how much each entry is trusted.
#[test]
fn a_trusted_key_file_filters_unless_audit_mode_and_every_verb_shows_trust() {
    let (_d, svc) = rig();
    let key = [7u8; 32];
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&key).verifying_key().to_bytes();
    let signed = SignCfg::new(Some(key), Some(vec![pubkey]), true);
    let on = |cfg: &SignCfg, verb: &str, p: Value, now: u64| run_full(&svc, "local", &format!("store.worktree.{verb}"), p, now, false, cfg, &Guard::from_runtime());
    on(&signed, "create", json!({"project": PROJ, "slug": "feat-t", "branch": "feat/t", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    // published by a daemon with no signing key: the entry is there but unsigned
    on(&SignCfg::off(), "publish", json!({"project": PROJ, "slug": "feat-t", "root": src.path().to_string_lossy(), "branch": "feat/t",
        "base": "abc", "head": "def", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}), 2).unwrap();

    // from_lookup would have made this filter; `require` here is what a trusted file alone gives
    let filtering = SignCfg::new(None, Some(vec![pubkey]), true);
    let audit = SignCfg::new(None, Some(vec![pubkey]), false);
    let s = on(&filtering, "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap();
    assert_eq!(s["hits"], json!([]), "unsigned entry hidden");
    assert_eq!(s["trust"]["feat-t"], "unsigned", "the trust level is shown even for what is filtered out");
    let s = on(&audit, "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap();
    assert_eq!(s["hits"].as_array().unwrap().len(), 1, "audit mode shows it");
    assert_eq!(s["trust"]["feat-t"], "unsigned", "and says why it is not to be trusted");
    let l = on(&audit, "leftovers", json!({"project": PROJ}), 3).unwrap();
    assert_eq!(l["trust"]["feat-t"], "unsigned");
    let st = src.path().join("f.json");
    let f = on(&audit, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 4).unwrap();
    assert_eq!(f["trust"]["feat-t"], "unsigned");
    // no trusted file: no trust field content, behaviour as before
    assert_eq!(on(&SignCfg::off(), "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap()["trust"], json!({}));

    // a signed republish is verified, and the follower's state file remembers the generation
    on(&signed, "publish", json!({"project": PROJ, "slug": "feat-t", "root": src.path().to_string_lossy(), "branch": "feat/t",
        "base": "abc", "head": "def2", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}), 5).unwrap();
    let st2 = src.path().join("f2.json");
    let f = on(&filtering, "follow", json!({"project": PROJ, "state": st2.to_string_lossy()}), 6).unwrap();
    assert_eq!(f["trust"]["feat-t"], "verified");
    assert!(std::fs::read_to_string(&st2).unwrap().contains("\"generations\""));
}

#[test]
fn a_worktree_store_over_its_quota_refuses_another_publish() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-q", "branch": "feat/q", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let publish = |extra: Value, now| {
        let mut p = json!({"project": PROJ, "slug": "feat-q", "root": src.path().to_string_lossy(), "branch": "feat/q", "base": "abc",
            "head": "def", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(&svc, "publish", p, now)
    };
    publish(json!({}), 2).unwrap();
    let e = publish(json!({"quota_bytes": 10}), 3).unwrap_err();
    assert!(e.contains("over its 10 byte quota"), "{e}");
    publish(json!({"quota_bytes": 0}), 4).unwrap();
}

#[test]
fn follow_retry_is_off_by_default_and_validated() {
    let with = |v: Option<&'static str>| SignCfg::from_lookup(move |k| (k == "WEFTOS_WT_FOLLOW_RETRY_MS").then(|| v.map(String::from)).flatten());
    assert_eq!(with(None).unwrap().follow_retry_ms, 0, "off unless the owner picks option B");
    assert_eq!(with(Some("")).unwrap().follow_retry_ms, 0);
    assert_eq!(with(Some("750")).unwrap().follow_retry_ms, 750);
    for bad in ["-1", "fast", "10001", "1.5"] {
        assert!(with(Some(bad)).err().unwrap().contains("WEFTOS_WT_FOLLOW_RETRY_MS"), "{bad}");
    }
}

#[test]
fn landing_can_be_previewed_and_the_conflict_policy_and_size_cap_are_chosen_per_land() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-c", "branch": "feat/c", "base": "abc"}), 1).unwrap();
    let n = names("local", PROJ, "feat-c").unwrap();
    let b = Op(svc.operator_objects());
    S1::put(&*svc.operator_objects(), &n.pr, "assets/logo.png", b"v1", false).unwrap();
    let ov = weftos_store::overlay::Overlay::new(&b, &n.wt, &n.pr);
    ov.put("assets/logo.png", b"mine").unwrap();
    ov.put("assets/new.bin", &[1u8; 300]).unwrap();
    S1::put(&*svc.operator_objects(), &n.pr, "assets/logo.png", b"teammate", false).unwrap();
    let p = |extra: Value| {
        let mut base = json!({"project": PROJ, "slug": "feat-c", "agent": "bob", "host": "m2"});
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };

    let pv = call(&svc, "promote", p(json!({"dry_run": true})), 10).unwrap();
    assert_eq!(pv["dry_run"], true);
    assert_eq!(pv["preview"]["conflicts"], json!([["assets/logo.png", "both_edited"]]));
    assert_eq!(pv["preview"]["would_promote"], json!(["assets/new.bin"]));
    assert!(weftos_store::promote::is_promoted(&b, &n.wt).unwrap().is_none(), "a preview lands nothing");

    let e = call(&svc, "promote", p(json!({})), 11).unwrap_err();
    assert!(e.contains("conflict") && e.contains("assets/logo.png"), "{e}");
    assert!(call(&svc, "promote", p(json!({"on_conflict": "bogus"})), 12).is_err());
    let cap = call(&svc, "promote", p(json!({"on_conflict": "keep_project", "max_bytes": 100})), 13).unwrap_err();
    assert!(cap.contains("byte cap"), "{cap}");

    let r = call(&svc, "promote", p(json!({"on_conflict": "keep_project", "max_bytes": 0})), 14).unwrap();
    assert_eq!(r["promoted"], json!(["assets/new.bin"]));
    assert_eq!(S1::get(&*svc.operator_objects(), &n.pr, "assets/logo.png").unwrap(), b"teammate", "the project version was kept");
}

/// The daemon's real ed25519 signer and trusted-key verifier against a real RustFS (the bytes the
/// store holds are what is signed). Throwaway RustFS only: WEFTOS_STORE_TEST_RUSTFS_ENV.
#[test]
#[ignore = "needs a throwaway RustFS: WEFTOS_STORE_TEST_RUSTFS_ENV"]
fn live_ed25519_signed_entries_verify_against_the_bytes_rustfs_holds() {
    use weftos_store::backend::AdminBackend;
    use weftos_store::registry_sign::{Trust, list_trusted_lenient, verify_registry_entry};
    let f = std::env::var("WEFTOS_STORE_TEST_RUSTFS_ENV").expect("WEFTOS_STORE_TEST_RUSTFS_ENV");
    let d = tempfile::tempdir().unwrap();
    let cfg = weftos_store::RustfsConfig::from_env_file(Path::new(&f), d.path().join("secrets")).unwrap();
    let be = weftos_store::RustfsBackend::new(cfg).unwrap();
    let b = format!("p-{}", ulid::Ulid::new().to_string().to_lowercase());
    be.create_bucket(&b, &[]).unwrap();

    let daemon = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
    let intruder = ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]);
    let trusted = [daemon.verifying_key().to_bytes()];
    let entry = |slug: &str| RegistryEntry {
        slug: slug.into(),
        bucket: weftos_store::share_fetch::expected_worktree_bucket(&b, slug).unwrap(),
        branch: "feat/x".into(),
        head_commit: "abc".into(),
        state: weftos_store::share::State::Open,
        writer: Actor { agent: "a".into(), host: "h".into() },
        merger: None,
        doc_count: 1,
        file_count: 1,
        updated_ms: 5,
        orphan: None,
        writer_check: None,
    };
    write_registry_entry_signed(&be, &b, &entry("good"), &Ed(daemon.clone())).unwrap();
    write_registry_entry_signed(&be, &b, &entry("forged"), &Ed(intruder)).unwrap();
    write_registry_entry(&be, &b, &entry("plain")).unwrap();

    let v = Trusted(&trusted);
    assert_eq!(verify_registry_entry(&be, &b, "good", &v).unwrap(), Trust::Verified);
    assert_eq!(verify_registry_entry(&be, &b, "forged", &v).unwrap(), Trust::Bad, "signed by a key nobody trusts");
    assert_eq!(verify_registry_entry(&be, &b, "plain", &v).unwrap(), Trust::Unsigned);
    let (ok, refused) = list_trusted_lenient(&be, &b, &v, true).unwrap();
    assert_eq!(ok.len(), 1);
    assert_eq!(refused, ["forged", "plain"]);

    // an edit of the stored entry breaks the signature
    let mut e = entry("good");
    e.head_commit = "edited".into();
    write_registry_entry(&be, &b, &e).unwrap();
    assert_eq!(verify_registry_entry(&be, &b, "good", &v).unwrap(), Trust::Bad);

    for s in ["good", "forged", "plain"] {
        remove_registry_entry_signed(&be, &b, s).unwrap();
    }
    // the daemon-written digest is not an entry: sweep what is left before the bucket can go
    for m in weftos_store::backend::ObjectBackend::list(&be, &b, "").unwrap() {
        weftos_store::backend::ObjectBackend::delete(&be, &b, &m.key).unwrap();
    }
    be.delete_bucket(&b).unwrap();
}

#[test]
fn signing_settings_come_from_files_and_a_bad_file_is_an_error_not_unsigned() {
    let d = tempfile::tempdir().unwrap();
    let k = d.path().join("k");
    let t = d.path().join("t");
    std::fs::write(&k, format!("{}\n", "ab".repeat(32))).unwrap();
    // the seed file must be private; make it so (a fresh file is typically 0644)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    std::fs::write(&t, format!("# trusted\n{}\n", "cd".repeat(32))).unwrap();
    let env = |k: &'static str, t: &'static str, r: Option<&'static str>| {
        let (k, t) = (k.to_owned(), t.to_owned());
        move |v: &str| match v {
            "WEFTOS_WT_REGISTRY_SIGN_KEY" => (!k.is_empty()).then(|| k.clone()),
            "WEFTOS_WT_REGISTRY_TRUSTED" => (!t.is_empty()).then(|| t.clone()),
            "WEFTOS_WT_REGISTRY_REQUIRE_SIGNED" => r.map(str::to_owned),
            _ => None,
        }
    };
    let (ks, ts) = (k.to_string_lossy().into_owned(), t.to_string_lossy().into_owned());
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let (ks, ts) = (leak(ks), leak(ts));
    #[cfg(unix)]
    {
        assert!(SignCfg::from_lookup(env(ks, ts, Some("1"))).err().unwrap().contains("accessible to other users"), "a world-readable seed is refused");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&k, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let c = SignCfg::from_lookup(env(ks, ts, Some("1"))).unwrap();
    assert!(c.key.is_some() && c.trusted.as_ref().unwrap().len() == 1 && c.require);
    std::fs::write(&k, "tooshort").unwrap();
    assert!(SignCfg::from_lookup(env(ks, ts, None)).is_err());
    let missing = leak(d.path().join("missing").to_string_lossy().into_owned());
    assert!(SignCfg::from_lookup(env(missing, ts, None)).err().unwrap().contains("missing"));
    assert!(SignCfg::from_lookup(env("", "", Some("1"))).is_err(), "require without a trusted file is refused");
    assert!(SignCfg::from_lookup(env("", "", None)).unwrap().key.is_none());
    // a trusted-key file filters by itself; `0` is the explicit audit mode
    assert!(SignCfg::from_lookup(env("", ts, None)).unwrap().require, "trusted file implies filtering");
    assert!(!SignCfg::from_lookup(env("", ts, Some("0"))).unwrap().require);
    assert!(SignCfg::from_lookup(env("", "", Some("0"))).unwrap().trusted.is_none());
}

#[test]
fn usage_shows_each_worktree_and_gc_retires_only_landed_ones_after_the_keep_period_and_only_when_confirmed() {
    let (_d, svc) = rig();
    let day = weftos_store::retention::DAY_MS;
    for s in ["feat-old", "feat-new", "feat-open"] {
        call(&svc, "create", json!({"project": PROJ, "slug": s, "branch": s, "base": "abc"}), 1).unwrap();
        let n = names("local", PROJ, s).unwrap();
        weftos_store::overlay::Overlay::new(&Op(svc.operator_objects()), &n.wt, &n.pr).put(&format!("assets/{s}.bin"), &[9u8; 40]).unwrap();
    }
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-old"}), 2 * day).unwrap();
    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-new"}), 20 * day).unwrap();
    let now = 21 * day;

    let u = call(&svc, "usage", json!({"project": PROJ, "quota_bytes": 100}), now).unwrap();
    let wts = u["worktrees"].as_array().unwrap();
    assert_eq!(wts.len(), 3);
    assert!(wts.iter().all(|w| w["over_quota"] == true), "each holds meta, base and 40 bytes: {u}");
    assert!(u["project"]["bytes"].as_u64().unwrap() >= 80, "two landed assets");
    let landed: Vec<_> = wts.iter().filter(|w| !w["landed_ms"].is_null()).collect();
    assert_eq!(landed.len(), 2);
    // the default quota is 8 GiB (twice the 4 GiB tree ceiling): small stores are under it, and the
    // reported figure is the one `publish` enforces
    let d = call(&svc, "usage", json!({"project": PROJ}), now).unwrap();
    assert_eq!(d["quota_bytes"], json!(8u64 << 30), "{d}");
    assert!(d["worktrees"].as_array().unwrap().iter().all(|w| w["over_quota"] == false), "{d}");

    // dry run is the default and changes nothing
    let g = call(&svc, "gc", json!({"project": PROJ}), now).unwrap();
    assert_eq!(g["dry_run"], true);
    let retire = g["retire"].as_array().unwrap();
    assert_eq!(retire.len(), 1, "feat-old landed 19 days ago; feat-new 1 day ago; feat-open never: {g}");
    assert!(retire[0].as_str().unwrap().ends_with("/worktrees/feat-old/store"));
    assert_eq!(call(&svc, "usage", json!({"project": PROJ}), now).unwrap()["worktrees"].as_array().unwrap().len(), 3);

    let g = call(&svc, "gc", json!({"project": PROJ, "confirmed": true}), now).unwrap();
    assert_eq!(g["retired"].as_array().unwrap().len(), 1);
    assert_eq!(g["failed"], json!([]));
    let left = call(&svc, "usage", json!({"project": PROJ}), now).unwrap();
    assert_eq!(left["worktrees"].as_array().unwrap().len(), 2, "the open worktree and the recent landing remain");
    // what landed is still in the project, and the retired store's contents are archived
    let nm = names("local", PROJ, "feat-old").unwrap();
    assert_eq!(S1::get(&*svc.operator_objects(), &nm.pr, "assets/feat-old.bin").unwrap(), vec![9u8; 40]);
    let arch = S1::list(&*svc.operator_objects(), ARCHIVE_BUCKET, &format!("{}@", nm.wt)).unwrap();
    assert!(!arch.is_empty(), "retired store archived before deletion");

    // archive expiry is opt-in; with a short keep period the old archived objects go
    let none = call(&svc, "gc", json!({"project": PROJ}), 400 * day).unwrap();
    assert_eq!(none["expire_archived"], 0, "no archive_keep_days: the archive is untouched");
    let ex = call(&svc, "gc", json!({"project": PROJ, "archive_keep_days": 30, "confirmed": true}), 400 * day).unwrap();
    assert!(ex["expired_archived"].as_u64().unwrap() > 0, "{ex}");
    // keys archived at the promotion time (2 days in) are gone; the retire copies carry the service's
    // real clock (newer than this test's cutoff) and stay
    let rest = S1::list(&*svc.operator_objects(), ARCHIVE_BUCKET, &format!("{}@", nm.wt)).unwrap();
    assert!(rest.iter().all(|m| weftos_store::retention::archive_key_parts(&m.key).is_some_and(|(_, ts)| ts > 400 * day - 30 * day)), "{rest:?}");
}

#[test]
fn with_signing_on_the_daemon_signs_and_a_strict_follower_hides_forged_entries() {
    let (_d, svc) = rig();
    let key = [7u8; 32];
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&key).verifying_key().to_bytes();
    let strict = SignCfg::new(Some(key), Some(vec![pubkey]), true);
    let other = SignCfg::new(None, Some(vec![[9u8; 32]]), true); // trusts a different key
    let on = |cfg: &SignCfg, verb: &str, p: Value, now: u64| run_full(&svc, "local", &format!("store.worktree.{verb}"), p, now, false, cfg, &Guard::from_runtime());
    on(&strict, "create", json!({"project": PROJ, "slug": "feat-s", "branch": "feat/s", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let pubp = json!({"project": PROJ, "slug": "feat-s", "root": src.path().to_string_lossy(), "branch": "feat/s", "base": "abc",
        "head": "def", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg});
    on(&strict, "publish", pubp, 2).unwrap();

    let n = names("local", PROJ, "feat-s").unwrap();
    let b = Op(svc.operator_objects());
    assert!(S1::get(&*svc.operator_objects(), &n.reg, "worktrees.sig/feat-s.json").is_ok(), "the daemon wrote a signature");

    let hits = |cfg: &SignCfg| on(cfg, "search", json!({"project": PROJ, "query": "plan"}), 3).unwrap()["hits"].as_array().unwrap().len();
    assert_eq!(hits(&strict), 1, "a follower trusting the daemon key sees it");
    assert_eq!(hits(&other), 0, "a follower trusting another key refuses it");
    assert_eq!(hits(&SignCfg::off()), 1, "no trust config: behaves as before");

    // someone with project write redirects the entry; the old signature no longer matches
    let mut e = weftos_store::share_fetch::list_shared(&b, &n.reg).unwrap().remove(0);
    e.head_commit = "forged".into();
    weftos_store::share_fetch::write_registry_entry(&b, &n.reg, &e).unwrap();
    assert_eq!(hits(&strict), 0, "an edited entry is hidden from a strict follower");
    let st = src.path().join("f.json");
    let f = on(&strict, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 4).unwrap();
    assert_eq!(f["changes"][0], json!({"change": "unreadable", "slug": "feat-s"}), "refused, and said so");
}

#[test]
fn with_the_registry_switch_on_entries_live_in_a_store_only_the_daemon_writes() {
    let (_d, svc) = rig();
    let run_on = |verb: &str, p: Value, now: u64| run_full(&svc, "local", &format!("store.worktree.{verb}"), p, now, true, &SignCfg::off(), &Guard::from_runtime());
    run_on("create", json!({"project": PROJ, "slug": "feat-r", "branch": "feat/r", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    run_on(
        "publish",
        json!({"project": PROJ, "slug": "feat-r", "root": src.path().to_string_lossy(), "branch": "feat/r", "base": "abc", "head": "def",
               "paths_z": paths, "uncommitted_z": unc, "changes_z": chg}),
        2,
    )
    .unwrap();
    let on = names_with("local", PROJ, "feat-r", true).unwrap();
    let b = Op(svc.operator_objects());
    assert_eq!(weftos_store::share_fetch::list_shared(&b, &on.reg).unwrap().len(), 1, "registry is in its own store");
    assert!(weftos_store::share_fetch::list_shared(&b, &on.pr).unwrap().is_empty(), "nothing in the project store");
    // a follower sees it through the verb
    let st = src.path().join("f.json");
    let f = run_on("follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 3).unwrap();
    assert_eq!(f["changes"][0]["change"], "new");
    // a governed principal cannot write into the registry store (no grants of any kind on it)
    let who = weftos_store::service::Caller::local(weftos_store::Principal::parse("agent:mallory").unwrap());
    let rn = weftos_store::StoreName::release("local", PROJ, REGISTRY_CHANNEL).unwrap();
    assert!(svc.put(&who, &rn, "worktrees/feat-r.json", b"{}", false).is_err());
    // default (switch off) keeps the registry in the project store, as before
    let off = names_with("local", PROJ, "feat-r", false).unwrap();
    assert_eq!(off.reg, off.pr);
}
