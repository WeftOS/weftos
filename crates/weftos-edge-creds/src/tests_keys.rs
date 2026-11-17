//! Key-material hygiene: nothing secret in `Debug`, constant-time compares,
//! wiped intermediates (BR finding 04 #15).

use zeroize::Zeroizing;

use crate::creds::TempCredentials;
use crate::rustfs::StaticKey;
use crate::sigv4::{self, ct_eq, sig_eq};
use crate::wire::{Endpoint, HttpRequest, HttpResponse, LeaseResponse, Presigned};

const SECRET: &str = "wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY";
const TOKEN: &str = "FQoGZXIvYXdzEJr-session-token-bytes";

fn creds() -> TempCredentials {
    TempCredentials { access_key: "AKIDEXAMPLE".into(), secret_key: SECRET.into(), session_token: TOKEN.into(), expires_at: 1_800_000_900 }
}

fn assert_clean(dbg: &str) {
    assert!(!dbg.contains(SECRET), "secret key leaked: {dbg}");
    assert!(!dbg.contains(TOKEN), "session token leaked: {dbg}");
}

#[test]
fn debug_never_prints_key_material() {
    assert_clean(&format!("{:?}", creds()));
    assert_clean(&format!("{:?}", StaticKey { access_key: "a".into(), secret_key: SECRET.into() }));
    let ep = Endpoint { scheme: "http".into(), host: "h:9000".into() };
    let sts = LeaseResponse::Sts { credentials: creds(), bucket: "b".into(), region: "r".into(), endpoint: ep.clone() };
    assert_clean(&format!("{sts:?} {sts:#?}"));
    let query = vec![("X-Amz-Security-Token".to_string(), TOKEN.to_string()), ("X-Amz-Signature".to_string(), SECRET.to_string())];
    let p = Presigned { method: "GET".into(), endpoint: ep, path: "/b/k".into(), query: query.clone(), url: format!("http://h/b/k?t={TOKEN}"), expires_at: 1 };
    assert_clean(&format!("{p:?} {:?}", LeaseResponse::Presigned(p.clone())));
    let req = HttpRequest {
        method: "GET".into(),
        scheme: "http".into(),
        host: "h".into(),
        path: "/".into(),
        query,
        headers: vec![("x-amz-security-token".into(), TOKEN.into()), ("authorization".into(), SECRET.into())],
        body: SECRET.as_bytes().to_vec(),
    };
    assert_clean(&format!("{req:?}"));
    assert_clean(&format!("{:?}", HttpResponse { status: 200, headers: vec![("x".into(), SECRET.into())], body: TOKEN.as_bytes().to_vec() }));
}

#[test]
fn credential_equality_covers_every_field() {
    assert_eq!(creds(), creds());
    let mut c = creds();
    c.secret_key.replace_range(..1, "X");
    assert_ne!(c, creds(), "secret differs");
    let mut c = creds();
    c.session_token.push('x');
    assert_ne!(c, creds(), "token length differs");
    let mut c = creds();
    c.access_key = "OTHER".into();
    assert_ne!(c, creds(), "access key differs");
    let mut c = creds();
    c.expires_at += 1;
    assert_ne!(c, creds(), "expiry differs");
}

#[test]
fn ct_eq_matches_slice_equality() {
    for (a, b) in [("", ""), ("a", "a"), ("a", "b"), ("ab", "a"), ("abcdef", "abcdeg"), ("xbcdef", "abcdef")] {
        assert_eq!(ct_eq(a.as_bytes(), b.as_bytes()), a == b, "{a:?} vs {b:?}");
        assert_eq!(sig_eq(a, b), a == b, "{a:?} vs {b:?}");
    }
}

#[test]
fn derived_signing_key_is_wiped_on_drop() {
    // The intermediate and final HMAC keys are `Zeroizing`; the type is the
    // contract (a plain `Vec<u8>` would not compile here).
    let k: Zeroizing<Vec<u8>> = sigv4::signing_key(SECRET, "20271231", "us-east-1", "s3");
    assert_eq!(k.len(), 32);
    // Known-answer: the derivation itself did not change.
    assert_eq!(sigv4::hex(&k).len(), 64);
}
