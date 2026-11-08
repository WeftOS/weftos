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
