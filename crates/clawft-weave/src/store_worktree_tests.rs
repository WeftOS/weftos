use std::sync::Arc;

use clawft_kernel::exposure::ExposureRegistry;
use serde_json::json;
use weftos_store::{FsBackend, StoreService};

use super::*;
use crate::store_worktree_paths::Guard;
use std::path::Path;
use crate::store_rt::{ActivitySink, ChainAudit, RegistryGate};

const PROJ: &str = "01JBXK2M3N4P5Q6R7S8T9V0W1X";

fn rig() -> (tempfile::TempDir, StoreService) {
    let d = tempfile::tempdir().unwrap();
    let reg = Arc::new(ExposureRegistry::new("pg-1"));
    let be = Arc::new(FsBackend::open(d.path()).unwrap());
    let svc = StoreService::open(
        weftos_store::service::Config { node_id: "pg-1".into(), state_path: None, clock: clawft_kernel::exposure::registry::now_ms },
        be.clone(),
        be,
        Arc::new(RegistryGate(reg.clone())),
        Arc::new(ActivitySink(reg)),
        Arc::new(ChainAudit(None)),
    )
    .unwrap();
    (d, svc)
}

fn call(svc: &StoreService, verb: &str, p: Value, now: u64) -> Result<Value, String> {
    run(svc, "local", &format!("store.worktree.{verb}"), p, now)
}

fn files(dir: &Path) -> (String, String, String) {
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("docs/plan.md"), "# Rooms plan\n").unwrap();
    std::fs::write(dir.join("a.rs"), "fn a() {}\n").unwrap();
    let w = |n: &str, c: &[u8]| {
        let f = dir.join(n);
        std::fs::write(&f, c).unwrap();
        f.to_string_lossy().into_owned()
    };
    (
        w("paths.z", b"docs/plan.md\0a.rs\0"),
        w("unc.z", b"docs/plan.md\0"),
        w("chg.z", b"M\0a.rs\0A\0docs/plan.md\0D\0gone.rs\0"),
    )
}

#[test]
fn the_verbs_carry_a_worktree_from_create_to_land_through_the_daemon_store() {
    let (_d, svc) = rig();
    let b = call(&svc, "buckets", json!({"project": PROJ, "slug": "feat-rooms"}), 1).unwrap();
    assert!(b["worktree"].as_str().unwrap().starts_with("w-01jbxk2m3n4p5q6r7s8t9v0w1x-"));
    assert_eq!(b["project"], "p-01jbxk2m3n4p5q6r7s8t9v0w1x");

    call(&svc, "create", json!({"project": PROJ, "slug": "feat-rooms", "branch": "feat/rooms", "base": "abc"}), 10).unwrap();
    // assets the project already has are visible to the worktree with no copy
    let pb = b["project"].as_str().unwrap();
    S1::put(&*svc.operator_objects(), pb, "assets/hero.bin", b"HERO", false).unwrap();

    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let root = src.path().to_string_lossy().into_owned();
    let publish = |agent: &str, now| {
        call(
            &svc,
            "publish",
            json!({"project": PROJ, "slug": "feat-rooms", "root": root, "branch": "feat/rooms", "base": "abc", "head": "def",
                   "paths_z": paths, "uncommitted_z": unc, "changes_z": chg, "handoff": "please merge", "agent": agent, "host": "m1"}),
            now,
        )
    };
    let r = publish("alice", 20).unwrap();
    assert_eq!(r["uploaded"], 2);

    // a second machine follows, searches, fetches, claims
    let st = src.path().join("follow.json");
    let f = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 30).unwrap();
    assert_eq!(f["changes"][0]["change"], "new");
    let s = call(&svc, "search", json!({"project": PROJ, "query": "plan"}), 31).unwrap();
    assert_eq!(s["hits"][0]["path"], "docs/plan.md");
    let dest = tempfile::tempdir().unwrap();
    let fe = call(&svc, "fetch", json!({"project": PROJ, "slug": "feat-rooms", "dest": dest.path().to_string_lossy()}), 32).unwrap();
    assert_eq!(fe["written"], 2);
    assert_eq!(std::fs::read_to_string(dest.path().join("docs/plan.md")).unwrap(), "# Rooms plan\n");
    let c = call(&svc, "claim", json!({"project": PROJ, "slug": "feat-rooms", "agent": "bob", "host": "m2"}), 40).unwrap();
    assert_eq!(c["merger"], "bob@m2");
    let nm = names("local", PROJ, "feat-rooms").unwrap();
    let h = weftos_store::share::read_handoff(&Op(svc.operator_objects()), &nm.wt).unwrap().unwrap();
    assert_eq!(h.epoch, 1, "a claim bumps the fencing epoch");
    assert_eq!(c["epoch"], 1);
    assert!(publish("alice", 41).is_err(), "the writer cannot overwrite a claimed merge");
    let f2 = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 42).unwrap();
    assert_eq!(f2["changes"][0]["change"], "state_changed");

    // landing: the worktree's own asset reaches the project, the claim is closed as merged
    let n = names("local", PROJ, "feat-rooms").unwrap();
    weftos_store::overlay::Overlay::new(&Op(svc.operator_objects()), &n.wt, &n.pr).put("assets/room.bin", b"ROOM").unwrap();
    let stale = call(&svc, "promote", json!({"project": PROJ, "slug": "feat-rooms", "agent": "bob", "host": "m2", "epoch": 7}), 49);
    assert!(stale.unwrap_err().contains("stale"), "a land under the wrong claim epoch is refused");
    let p = call(&svc, "promote", json!({"project": PROJ, "slug": "feat-rooms", "agent": "bob", "host": "m2", "epoch": 1}), 50).unwrap();
    assert_eq!(p["promoted"], json!(["assets/room.bin"]));
    assert_eq!(S1::get(&*svc.operator_objects(), pb, "assets/room.bin").unwrap(), b"ROOM");
    // followers see the landed worktree as merged, not still claimed
    let f3 = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 52).unwrap();
    assert_eq!(f3["changes"][0]["change"], "state_changed");
    assert_eq!(f3["changes"][0]["worktree"]["state"], "merged");
    let again = call(&svc, "promote", json!({"project": PROJ, "slug": "feat-rooms"}), 51).unwrap();
    assert_eq!(again["promoted"], p["promoted"], "landing twice is a no-op");
    let re = call(&svc, "create", json!({"project": PROJ, "slug": "feat-rooms", "branch": "feat/rooms", "base": "abc"}), 60);
    assert!(re.unwrap_err().contains("already landed"), "a landed slug is not reused");
}

#[test]
fn bad_input_is_refused() {
    let (_d, svc) = rig();
    assert!(call(&svc, "buckets", json!({"project": PROJ, "slug": "Bad Slug"}), 1).is_err());
    assert!(call(&svc, "buckets", json!({"project": "../x", "slug": "ok"}), 1).is_err());
    assert!(call(&svc, "buckets", json!({"project": PROJ, "slug": "ok", "extra": 1}), 1).is_err(), "unknown params refused");
    assert!(call(&svc, "nope", json!({"project": PROJ}), 1).is_err());
    assert!(call(&svc, "publish", json!({"project": PROJ, "slug": "ok", "root": "relative/dir", "branch": "b", "base": "a", "head": "h"}), 1).is_err());
}

#[test]
fn fetch_writes_the_bundle_when_one_was_published_and_says_so_when_not() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-b", "branch": "feat/b", "base": "abc"}), 10).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    let root = src.path().to_string_lossy().into_owned();
    let publish = |extra: Value| {
        let mut p = json!({"project": PROJ, "slug": "feat-b", "root": root, "branch": "feat/b", "base": "abc", "head": "def",
                           "paths_z": paths, "uncommitted_z": unc, "changes_z": chg});
        p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        call(&svc, "publish", p, 20)
    };
    let dest = tempfile::tempdir().unwrap();
    let out = dest.path().join("nested/b.bundle");
    let fetch = || {
        call(&svc, "fetch", json!({"project": PROJ, "slug": "feat-b", "dest": dest.path().join("t").to_string_lossy(),
                                   "bundle_out": out.to_string_lossy()}), 30)
    };

    // no bundle published: a clear error, and no file written
    publish(json!({})).unwrap();
    let err = fetch().unwrap_err();
    assert!(err.contains("without a git bundle"), "{err}");
    assert!(!out.exists());

    // with a bundle: bytes land at the requested path (parent created), head returned
    let bundle = src.path().join("in.bundle");
    std::fs::write(&bundle, b"BUNDLE-BYTES").unwrap();
    publish(json!({"bundle": bundle.to_string_lossy()})).unwrap();
    let r = fetch().unwrap();
    assert_eq!(r["bundle_head"], "def");
    assert_eq!(std::fs::read(&out).unwrap(), b"BUNDLE-BYTES");
}

#[test]
fn with_the_registry_switch_on_entries_live_in_a_store_only_the_daemon_writes() {
    let (_d, svc) = rig();
    let run_on = |verb: &str, p: Value, now: u64| run_with(&svc, "local", &format!("store.worktree.{verb}"), p, now, true);
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

#[test]
fn a_landed_worktree_leaves_the_registry_so_followers_do_not_see_a_forgotten_claim() {
    let (_d, svc) = rig();
    call(&svc, "create", json!({"project": PROJ, "slug": "feat-l", "branch": "feat/l", "base": "abc"}), 1).unwrap();
    let src = tempfile::tempdir().unwrap();
    let (paths, unc, chg) = files(src.path());
    call(&svc, "publish", json!({"project": PROJ, "slug": "feat-l", "root": src.path().to_string_lossy(), "branch": "feat/l", "base": "abc",
        "head": "def", "paths_z": paths, "uncommitted_z": unc, "changes_z": chg, "handoff": "go"}), 2).unwrap();
    call(&svc, "claim", json!({"project": PROJ, "slug": "feat-l", "agent": "bob", "host": "m2"}), 3).unwrap();
    let st = src.path().join("f.json");
    let f = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy()}), 4).unwrap();
    assert_eq!(f["changes"][0]["change"], "new");

    call(&svc, "promote", json!({"project": PROJ, "slug": "feat-l", "agent": "bob", "host": "m2", "epoch": 1}), 5).unwrap();
    let day = 3 * 24 * 3600 * 1000;
    let left = call(&svc, "leftovers", json!({"project": PROJ}), day).unwrap();
    assert_eq!(left["leftovers"], json!([]), "a landed worktree is not an abandoned claim");
    let f = call(&svc, "follow", json!({"project": PROJ, "state": st.to_string_lossy(), "confirmed": true}), 6).unwrap();
    assert_eq!(f["changes"][0]["change"], "gone");
}

#[test]
fn with_signing_on_the_daemon_signs_and_a_strict_follower_hides_forged_entries() {
    let (_d, svc) = rig();
    let key = [7u8; 32];
    let pubkey = ed25519_dalek::SigningKey::from_bytes(&key).verifying_key().to_bytes();
    let strict = SignCfg::new(Some(key), Some(vec![pubkey]), true);
    let other = SignCfg::new(None, Some(vec![[9u8; 32]]), true); // trusts a different key
    let on = |cfg: &SignCfg, verb: &str, p: Value, now: u64| run_full(&svc, "local", &format!("store.worktree.{verb}"), p, now, false, cfg);
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
fn signing_settings_come_from_files_and_a_bad_file_is_an_error_not_unsigned() {
    let d = tempfile::tempdir().unwrap();
    let k = d.path().join("k");
    let t = d.path().join("t");
    std::fs::write(&k, format!("{}\n", "ab".repeat(32))).unwrap();
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
    let c = SignCfg::from_lookup(env(ks, ts, Some("1"))).unwrap();
    assert!(c.key.is_some() && c.trusted.as_ref().unwrap().len() == 1 && c.require);
    std::fs::write(&k, "tooshort").unwrap();
    assert!(SignCfg::from_lookup(env(ks, ts, None)).is_err());
    let missing = leak(d.path().join("missing").to_string_lossy().into_owned());
    assert!(SignCfg::from_lookup(env(missing, ts, None)).err().unwrap().contains("missing"));
    assert!(SignCfg::from_lookup(env("", "", Some("1"))).is_err(), "require without a trusted file is refused");
    assert!(SignCfg::from_lookup(env("", "", None)).unwrap().key.is_none());
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
        bucket: format!("w-01abc-{slug}"),
        branch: "feat/x".into(),
        head_commit: "abc".into(),
        state: weftos_store::share::State::Open,
        writer: Actor { agent: "a".into(), host: "h".into() },
        merger: None,
        doc_count: 1,
        file_count: 1,
        updated_ms: 5,
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
    be.delete_bucket(&b).unwrap();
}
