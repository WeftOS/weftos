//! Signed registry entries and claim epochs.
use sha2::{Digest, Sha256};
use weftos_store::overlay::ObjectBackend;
use weftos_store::registry_sign::*;
use weftos_store::share::*;
use weftos_store::share_fetch::*;
use weftos_store::testing::MemBackend;

const PR: &str = "p-01abc";

/// A keyed-hash stand-in for the daemon's real signer; the traits are what matter here.
struct Toy(&'static str);
impl EntrySigner for Toy {
    fn signer_id(&self) -> String {
        format!("toy:{}", self.0)
    }
    fn alg(&self) -> &'static str {
        "toy-sha256"
    }
    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        Sha256::new().chain_update(self.0).chain_update(msg).finalize().to_vec()
    }
}
struct Trusts(&'static str);
impl EntryVerifier for Trusts {
    fn verify(&self, signer: &str, alg: &str, msg: &[u8], sig: &[u8]) -> bool {
        signer == format!("toy:{}", self.0) && alg == "toy-sha256" && Toy(self.0).sign(msg) == sig
    }
}

fn entry(slug: &str) -> RegistryEntry {
    RegistryEntry {
        slug: slug.into(),
        bucket: format!("w-01abc-{slug}"),
        branch: "feat/x".into(),
        head_commit: "abc".into(),
        state: State::Open,
        writer: Actor { agent: "a".into(), host: "h".into() },
        merger: None,
        doc_count: 0,
        file_count: 0,
        updated_ms: 1,
    }
}

#[test]
fn signed_entries_verify_and_edits_forgeries_and_unsigned_are_told_apart() {
    let b = MemBackend::new();
    write_registry_entry_signed(&b, PR, &entry("good"), &Toy("daemon")).unwrap();
    write_registry_entry_signed(&b, PR, &entry("edited"), &Toy("daemon")).unwrap();
    write_registry_entry_signed(&b, PR, &entry("forged"), &Toy("intruder")).unwrap();
    write_registry_entry(&b, PR, &entry("plain")).unwrap();
    // someone with project write redirects an entry at another bucket, keeping the old signature
    let mut moved = entry("edited");
    moved.bucket = "w-01zzz-elsewhere".into();
    write_registry_entry(&b, PR, &moved).unwrap();

    let v = Trusts("daemon");
    let t = |s| verify_registry_entry(&b, PR, s, &v).unwrap();
    assert_eq!(t("good"), Trust::Verified);
    assert_eq!(t("edited"), Trust::Bad);
    assert_eq!(t("forged"), Trust::Bad);
    assert_eq!(t("plain"), Trust::Unsigned);

    let all = list_shared_verified(&b, PR, &v, false).unwrap();
    assert_eq!(all.len(), 4);
    let strict = list_shared_verified(&b, PR, &v, true).unwrap();
    assert_eq!(strict.len(), 1);
    assert_eq!(strict[0].0.slug, "good");
}

#[test]
fn a_mangled_signature_object_is_bad_not_an_error() {
    let b = MemBackend::new();
    write_registry_entry_signed(&b, PR, &entry("x"), &Toy("daemon")).unwrap();
    b.put(PR, "worktrees.sig/x.json", b"not json").unwrap();
    assert_eq!(verify_registry_entry(&b, PR, "x", &Trusts("daemon")).unwrap(), Trust::Bad);
    b.put(PR, "worktrees.sig/x.json", br#"{"signer":"toy:daemon","alg":"toy-sha256","sig":"zz"}"#).unwrap();
    assert_eq!(verify_registry_entry(&b, PR, "x", &Trusts("daemon")).unwrap(), Trust::Bad);
    assert!(verify_registry_entry(&b, PR, "../x", &Trusts("daemon")).is_err());
}

#[test]
fn a_strict_follower_is_told_about_refused_entries_and_removal_leaves_no_orphan() {
    use weftos_store::follower::{Change, FollowState};
    let b = MemBackend::new();
    write_registry_entry_signed(&b, PR, &entry("good"), &Toy("daemon")).unwrap();
    write_registry_entry_signed(&b, PR, &entry("forged"), &Toy("intruder")).unwrap();
    write_registry_entry(&b, PR, &entry("plain")).unwrap();
    let v = Trusts("daemon");

    let mut st = FollowState::default();
    let ch = poll_verified(&b, PR, &mut st, &v, true, false).unwrap();
    assert!(ch.iter().any(|c| matches!(c, Change::New(e) if e.slug == "good")));
    // refused, not silently absent
    for s in ["forged", "plain"] {
        assert!(ch.iter().any(|c| matches!(c, Change::Unreadable { slug } if slug == s)), "{s}: {ch:?}");
    }
    assert!(!st.seen.contains_key("forged") && !st.seen.contains_key("plain"));

    // lenient mode keeps everything and reports each entry's trust
    let (all, bad) = list_trusted_lenient(&b, PR, &v, false).unwrap();
    assert_eq!((all.len(), bad.len()), (3, 0));

    // an entry that was trusted and then loses its signature goes Unreadable and is remembered
    let mut st = FollowState::default();
    poll_verified(&b, PR, &mut st, &v, true, false).unwrap();
    let mut edited = entry("good");
    edited.head_commit = "moved".into();
    write_registry_entry(&b, PR, &edited).unwrap(); // rewritten without a fresh signature
    let ch = poll_verified(&b, PR, &mut st, &v, true, false).unwrap();
    assert!(ch.iter().any(|c| matches!(c, Change::Unreadable { slug } if slug == "good")), "{ch:?}");
    assert!(st.seen.contains_key("good"), "previous state kept, so it is not reported as gone");

    remove_registry_entry_signed(&b, PR, "good").unwrap();
    assert!(b.get(PR, "worktrees/good.json").unwrap().is_none());
    assert!(b.get(PR, "worktrees.sig/good.json").unwrap().is_none());
    // removing an unsigned entry (no sidecar) is fine too
    remove_registry_entry_signed(&b, PR, "plain").unwrap();
}

/// S3 review S2 #1: an object under one key whose body names another (signed) worktree must not be
/// listed, let alone vouched for by that worktree's signature.
#[test]
fn an_entry_whose_body_names_another_worktree_is_refused_not_vouched_for() {
    use weftos_store::follower::{Change, FollowState};
    let b = MemBackend::new();
    write_registry_entry_signed(&b, PR, &entry("good"), &Toy("daemon")).unwrap();
    // someone with project write plants worktrees/zzz.json whose body is a redirected copy of "good"
    let mut evil = entry("good");
    evil.bucket = "w-EVIL".into();
    b.put(PR, "worktrees/zzz.json", &serde_json::to_vec_pretty(&evil).unwrap()).unwrap();
    // and even copies good's genuine bytes and signature under the new key
    let good_bytes = b.get(PR, "worktrees/good.json").unwrap().unwrap();
    b.put(PR, "worktrees/yyy.json", &good_bytes).unwrap();
    b.put(PR, "worktrees.sig/yyy.json", &b.get(PR, "worktrees.sig/good.json").unwrap().unwrap()).unwrap();
    let v = Trusts("daemon");

    let (listed, refused) = list_trusted_lenient(&b, PR, &v, true).unwrap();
    assert_eq!(listed.iter().map(|(e, _)| e.slug.as_str()).collect::<Vec<_>>(), ["good"]);
    assert_eq!(refused, ["yyy", "zzz"], "named, not silently dropped");
    // not even a lenient (non-strict) listing shows a mislabelled entry
    let (listed, refused) = list_trusted_lenient(&b, PR, &v, false).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(refused, ["yyy", "zzz"]);
    assert_eq!(verify_registry_entry(&b, PR, "zzz", &v).unwrap(), Trust::Bad);
    assert_eq!(verify_registry_entry(&b, PR, "yyy", &v).unwrap(), Trust::Bad);
    assert_eq!(list_shared_verified(&b, PR, &v, false).unwrap().len(), 1);
    // the unsigned paths refuse them too
    let (plain, bad) = list_shared_lenient(&b, PR).unwrap();
    assert_eq!((plain.len(), bad.as_slice()), (1, ["yyy".to_string(), "zzz".to_string()].as_slice()));
    assert!(list_shared(&b, PR).is_err());

    // a follower is told, and the forged bucket is never aimed at
    let mut st = FollowState::default();
    let ch = poll_verified(&b, PR, &mut st, &v, true, false).unwrap();
    assert!(ch.iter().any(|c| matches!(c, Change::New(e) if e.slug == "good" && e.bucket != "w-EVIL")));
    for s in ["yyy", "zzz"] {
        assert!(ch.iter().any(|c| matches!(c, Change::Unreadable { slug } if slug == s)), "{ch:?}");
    }
    assert!(st.seen.values().all(|s| s.bucket != "w-EVIL"));
}

// ---- S3 review S2 #3: domain, bucket/slug binding, generation (rollback) ------------------------

/// Copy a signed entry and its signature object from one bucket to another, byte for byte.
fn replay(b: &MemBackend, from: &str, to: &str, slug: &str) {
    for k in [format!("worktrees/{slug}.json"), format!("worktrees.sig/{slug}.json")] {
        let d = b.get(from, &k).unwrap().unwrap();
        b.put(to, &k, &d).unwrap();
    }
}

#[test]
fn the_signed_message_is_domain_prefixed_and_bound_to_bucket_slug_and_generation() {
    let m = signing_message("p-01abc", "feat-x", 7, b"{\"a\":1}");
    assert_eq!(m, b"weftos-registry-v1\0p-01abc\0feat-x\x007\x00{\"a\":1}".to_vec());
    // a verifier sees exactly this, never the bare entry bytes
    struct Spy(std::cell::RefCell<Vec<Vec<u8>>>);
    impl EntryVerifier for Spy {
        fn verify(&self, _: &str, _: &str, msg: &[u8], _: &[u8]) -> bool {
            self.0.borrow_mut().push(msg.to_vec());
            true
        }
    }
    let b = MemBackend::new();
    write_registry_entry_signed(&b, PR, &entry("x"), &Toy("daemon")).unwrap();
    let spy = Spy(Default::default());
    assert_eq!(verify_registry_entry(&b, PR, "x", &spy).unwrap(), Trust::Verified);
    let seen = spy.0.borrow();
    assert!(seen[0].starts_with(b"weftos-registry-v1\0p-01abc\0x\0"), "{:?}", String::from_utf8_lossy(&seen[0]));
    // a signature over the bare bytes (what v0 signed) is no longer accepted
    let bytes = b.get(PR, "worktrees/x.json").unwrap().unwrap();
    let old = serde_json::json!({"signer": "toy:daemon", "alg": "toy-sha256", "generation": 1, "sig": hex_of(&Toy("daemon").sign(&bytes))});
    b.put(PR, "worktrees.sig/x.json", old.to_string().as_bytes()).unwrap();
    assert_eq!(verify_registry_entry(&b, PR, "x", &Trusts("daemon")).unwrap(), Trust::Bad);
    // and a signature object from before generations existed is Bad, not an error
    b.put(PR, "worktrees.sig/x.json", br#"{"signer":"toy:daemon","alg":"toy-sha256","sig":"00"}"#).unwrap();
    assert_eq!(verify_registry_entry(&b, PR, "x", &Trusts("daemon")).unwrap(), Trust::Bad);
}

fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn a_signed_entry_replayed_into_another_project_or_under_another_name_does_not_verify() {
    let b = MemBackend::new();
    write_registry_entry_signed(&b, PR, &entry("good"), &Toy("daemon")).unwrap();
    replay(&b, PR, "p-01zzz", "good");
    let v = Trusts("daemon");
    assert_eq!(verify_registry_entry(&b, PR, "good", &v).unwrap(), Trust::Verified);
    assert_eq!(verify_registry_entry(&b, "p-01zzz", "good", &v).unwrap(), Trust::Bad, "another project's registry");
    // same bucket, other name: the body names "good" so it is refused outright (and its signature
    // object, copied under the new name, would not verify either)
    let bytes = b.get(PR, "worktrees/good.json").unwrap().unwrap();
    b.put(PR, "worktrees/other.json", &bytes).unwrap();
    b.put(PR, "worktrees.sig/other.json", &b.get(PR, "worktrees.sig/good.json").unwrap().unwrap()).unwrap();
    assert_eq!(verify_registry_entry(&b, PR, "other", &v).unwrap(), Trust::Bad);
}

#[test]
fn generations_increase_with_every_signed_write_even_at_the_same_timestamp() {
    let b = MemBackend::new();
    let gen_of = |b: &MemBackend| -> u64 {
        let d = b.get(PR, "worktrees.sig/x.json").unwrap().unwrap();
        serde_json::from_slice::<serde_json::Value>(&d).unwrap()["generation"].as_u64().unwrap()
    };
    write_registry_entry_signed(&b, PR, &entry("x"), &Toy("daemon")).unwrap();
    let g1 = gen_of(&b);
    write_registry_entry_signed(&b, PR, &entry("x"), &Toy("daemon")).unwrap();
    let g2 = gen_of(&b);
    assert!(g2 > g1, "{g1} -> {g2}");
    // removed and written again later: still above the old one when time moved on
    remove_registry_entry_signed(&b, PR, "x").unwrap();
    let mut later = entry("x");
    later.updated_ms = 10_000;
    write_registry_entry_signed(&b, PR, &later, &Toy("daemon")).unwrap();
    assert!(gen_of(&b) > g2);
}

#[test]
fn a_follower_refuses_an_older_signed_entry_put_back_after_a_newer_one() {
    use weftos_store::follower::{Change, FollowState};
    let b = MemBackend::new();
    let v = Trusts("daemon");
    let mut e = entry("wt");
    e.state = State::Claimed;
    e.updated_ms = 100;
    write_registry_entry_signed(&b, PR, &e, &Toy("daemon")).unwrap();
    let (old_entry, old_sig) = (b.get(PR, "worktrees/wt.json").unwrap().unwrap(), b.get(PR, "worktrees.sig/wt.json").unwrap().unwrap());

    let mut st = FollowState::default();
    assert!(matches!(&poll_verified(&b, PR, &mut st, &v, true, false).unwrap()[..], [Change::New(x)] if x.state == State::Claimed));
    e.state = State::Merged;
    e.updated_ms = 200;
    write_registry_entry_signed(&b, PR, &e, &Toy("daemon")).unwrap();
    assert!(matches!(&poll_verified(&b, PR, &mut st, &v, true, false).unwrap()[..], [Change::StateChanged { entry, .. }] if entry.state == State::Merged));

    // an intruder with project write puts the older, genuinely signed Claimed entry back
    b.put(PR, "worktrees/wt.json", &old_entry).unwrap();
    b.put(PR, "worktrees.sig/wt.json", &old_sig).unwrap();
    assert_eq!(verify_registry_entry(&b, PR, "wt", &v).unwrap(), Trust::Verified, "the signature itself is genuine");
    let ch = poll_verified(&b, PR, &mut st, &v, true, false).unwrap();
    assert!(matches!(&ch[..], [Change::Unreadable { slug }] if slug == "wt"), "refused as a rollback: {ch:?}");
    assert_eq!(st.seen["wt"].state, State::Merged, "the follower keeps what it knew");

    // the same after the worktree has left the registry: the remembered generation outlives it
    remove_registry_entry_signed(&b, PR, "wt").unwrap();
    poll_verified(&b, PR, &mut st, &v, true, true).unwrap();
    b.put(PR, "worktrees/wt.json", &old_entry).unwrap();
    b.put(PR, "worktrees.sig/wt.json", &old_sig).unwrap();
    let ch = poll_verified(&b, PR, &mut st, &v, true, false).unwrap();
    assert!(ch.iter().all(|c| !matches!(c, Change::New(_))), "an old entry does not come back as new: {ch:?}");

    // the state file keeps the generations across runs; a follower with no history cannot know
    let json = serde_json::to_string(&st).unwrap();
    assert!(serde_json::from_str::<FollowState>(&json).unwrap().generations["wt"] > 0);
    let mut fresh = FollowState::default();
    assert!(matches!(&poll_verified(&b, PR, &mut fresh, &v, true, false).unwrap()[..], [Change::New(_)]), "documented limit");
}

#[test]
fn an_old_state_file_without_generations_still_loads() {
    use weftos_store::follower::FollowState;
    let st: FollowState = serde_json::from_str(r#"{"seen":{}}"#).unwrap();
    assert!(st.generations.is_empty());
}
