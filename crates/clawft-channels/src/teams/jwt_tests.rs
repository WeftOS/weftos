//! Tests for [`super::jwt::TeamsJwtVerifier`]. Keys are generated per run
//! and the JWKS comes from a stub; nothing touches the network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;

use super::jwt::*;

const APP_ID: &str = "11111111-2222-3333-4444-555555555555";
const JWKS_URL: &str = "https://login.botframework.com/v1/.well-known/keys";

struct TestKey {
    kid: &'static str,
    enc: EncodingKey,
    n: String,
    e: String,
}

fn make_key(kid: &'static str) -> TestKey {
    let mut rng = rand::thread_rng();
    let sk = RsaPrivateKey::new(&mut rng, 2048).expect("keygen");
    let der = sk.to_pkcs1_der().expect("der");
    TestKey {
        kid,
        enc: EncodingKey::from_rsa_der(der.as_bytes()),
        n: B64.encode(sk.n().to_bytes_be()),
        e: B64.encode(sk.e().to_bytes_be()),
    }
}

fn key_a() -> &'static TestKey {
    static K: OnceLock<TestKey> = OnceLock::new();
    K.get_or_init(|| make_key("key-a"))
}

/// Same kid as `key_a`, different key material.
fn key_a_impostor() -> &'static TestKey {
    static K: OnceLock<TestKey> = OnceLock::new();
    K.get_or_init(|| make_key("key-a"))
}

fn key_b() -> &'static TestKey {
    static K: OnceLock<TestKey> = OnceLock::new();
    K.get_or_init(|| make_key("key-b"))
}

fn jwks_doc(keys: &[&TestKey]) -> serde_json::Value {
    let list: Vec<_> = keys
        .iter()
        .map(|k| {
            serde_json::json!({"kty":"RSA","use":"sig","kid":k.kid,"n":k.n,"e":k.e,
                               "endorsements":["msteams"]})
        })
        .collect();
    serde_json::json!({ "keys": list })
}

#[derive(Clone)]
struct StubSource {
    jwks: Arc<Mutex<serde_json::Value>>,
    fetches: Arc<AtomicUsize>,
}

impl StubSource {
    fn new(keys: &[&TestKey]) -> Self {
        Self {
            jwks: Arc::new(Mutex::new(jwks_doc(keys))),
            fetches: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl JwksSource for StubSource {
    async fn get_json(&self, url: &str) -> Result<serde_json::Value, String> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        if url == BOT_FRAMEWORK_OPENID_URL {
            Ok(serde_json::json!({ "jwks_uri": JWKS_URL }))
        } else if url == JWKS_URL {
            Ok(self.jwks.lock().unwrap().clone())
        } else {
            Err(format!("unexpected url {url}"))
        }
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn claims(iss: &str, aud: &str, exp: i64, nbf: i64) -> serde_json::Value {
    serde_json::json!({"iss":iss,"aud":aud,"exp":exp,"nbf":nbf,"serviceurl":"https://smba.example/"})
}

fn good_claims() -> serde_json::Value {
    claims(BOT_FRAMEWORK_ISSUER, APP_ID, now() + 3600, now() - 60)
}

fn sign(key: &TestKey, kid: Option<&str>, c: &serde_json::Value) -> String {
    let mut h = Header::new(Algorithm::RS256);
    h.kid = kid.map(str::to_string);
    jsonwebtoken::encode(&h, c, &key.enc).expect("sign")
}

fn verifier(src: &StubSource) -> TeamsJwtVerifier {
    TeamsJwtVerifier::new(APP_ID, Box::new(src.clone())).unwrap()
}

/// Verifier over a stub JWKS plus a valid token for it (router-level tests).
pub(crate) fn fixture_verifier_and_token() -> (TeamsJwtVerifier, String) {
    let src = StubSource::new(&[key_a()]);
    let tok = sign(key_a(), Some("key-a"), &good_claims());
    (verifier(&src), tok)
}

#[tokio::test]
async fn valid_token_accepted() {
    let src = StubSource::new(&[key_a(), key_b()]);
    let tok = sign(key_a(), Some("key-a"), &good_claims());
    verifier(&src).verify(&tok).await.expect("valid token");
}

#[tokio::test]
async fn empty_app_id_refused() {
    let src = StubSource::new(&[key_a()]);
    assert!(TeamsJwtVerifier::new("", Box::new(src.clone())).is_err());
    assert!(TeamsJwtVerifier::new("  ", Box::new(src)).is_err());
}

#[tokio::test]
async fn wrong_key_same_kid_rejected() {
    let src = StubSource::new(&[key_a()]);
    let tok = sign(key_a_impostor(), Some("key-a"), &good_claims());
    assert!(verifier(&src).verify(&tok).await.is_err());
}

#[tokio::test]
async fn unknown_kid_rejected() {
    let src = StubSource::new(&[key_a()]);
    let tok = sign(key_b(), Some("key-b"), &good_claims());
    let err = verifier(&src).verify(&tok).await.unwrap_err();
    assert!(err.contains("unknown signing key"), "{err}");
}

#[tokio::test]
async fn missing_kid_rejected() {
    let src = StubSource::new(&[key_a()]);
    let tok = sign(key_a(), None, &good_claims());
    assert!(verifier(&src).verify(&tok).await.is_err());
}

#[tokio::test]
async fn wrong_audience_rejected() {
    let src = StubSource::new(&[key_a()]);
    let c = claims(BOT_FRAMEWORK_ISSUER, "someone-else", now() + 3600, now() - 60);
    let tok = sign(key_a(), Some("key-a"), &c);
    assert!(verifier(&src).verify(&tok).await.is_err());
}

#[tokio::test]
async fn wrong_issuer_rejected() {
    let src = StubSource::new(&[key_a()]);
    let c = claims("https://evil.example", APP_ID, now() + 3600, now() - 60);
    let tok = sign(key_a(), Some("key-a"), &c);
    assert!(verifier(&src).verify(&tok).await.is_err());
}

#[tokio::test]
async fn expired_rejected_beyond_skew_but_within_skew_accepted() {
    let src = StubSource::new(&[key_a()]);
    let v = verifier(&src);
    let old = claims(BOT_FRAMEWORK_ISSUER, APP_ID, now() - 3600, now() - 7200);
    assert!(v.verify(&sign(key_a(), Some("key-a"), &old)).await.is_err());
    let slightly = claims(BOT_FRAMEWORK_ISSUER, APP_ID, now() - 60, now() - 7200);
    v.verify(&sign(key_a(), Some("key-a"), &slightly)).await.expect("inside skew");
}

#[tokio::test]
async fn not_yet_valid_rejected_beyond_skew() {
    let src = StubSource::new(&[key_a()]);
    let v = verifier(&src);
    let future = claims(BOT_FRAMEWORK_ISSUER, APP_ID, now() + 7200, now() + 3600);
    assert!(v.verify(&sign(key_a(), Some("key-a"), &future)).await.is_err());
    let slightly = claims(BOT_FRAMEWORK_ISSUER, APP_ID, now() + 7200, now() + 60);
    v.verify(&sign(key_a(), Some("key-a"), &slightly)).await.expect("inside skew");
}

#[tokio::test]
async fn missing_exp_rejected() {
    let src = StubSource::new(&[key_a()]);
    let c = serde_json::json!({"iss":BOT_FRAMEWORK_ISSUER,"aud":APP_ID});
    assert!(verifier(&src).verify(&sign(key_a(), Some("key-a"), &c)).await.is_err());
}

#[tokio::test]
async fn alg_none_rejected() {
    let src = StubSource::new(&[key_a()]);
    let h = B64.encode(br#"{"alg":"none","typ":"JWT","kid":"key-a"}"#);
    let p = B64.encode(good_claims().to_string());
    let tok = format!("{h}.{p}.");
    assert!(verifier(&src).verify(&tok).await.is_err());
}

#[tokio::test]
async fn hs256_alg_confusion_rejected() {
    // Attacker signs HS256 using the public key material as the HMAC secret.
    let src = StubSource::new(&[key_a()]);
    let mut h = Header::new(Algorithm::HS256);
    h.kid = Some("key-a".into());
    let secret = EncodingKey::from_secret(key_a().n.as_bytes());
    let tok = jsonwebtoken::encode(&h, &good_claims(), &secret).unwrap();
    let err = verifier(&src).verify(&tok).await.unwrap_err();
    assert!(err.contains("RS256 only"), "{err}");
    // Rejected before any key fetch.
    assert_eq!(src.fetches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_tokens_rejected() {
    let src = StubSource::new(&[key_a()]);
    let v = verifier(&src);
    for bad in ["", "not-a-jwt", "a.b", "a.b.c", "....", "eyJhbGciOiJSUzI1NiJ9.e30."] {
        assert!(v.verify(bad).await.is_err(), "accepted {bad:?}");
    }
}

#[tokio::test]
async fn tampered_payload_rejected() {
    let src = StubSource::new(&[key_a()]);
    let tok = sign(key_a(), Some("key-a"), &good_claims());
    let parts: Vec<&str> = tok.split('.').collect();
    let forged = claims(BOT_FRAMEWORK_ISSUER, APP_ID, now() + 999_999, now() - 60);
    let tampered = format!("{}.{}.{}", parts[0], B64.encode(forged.to_string()), parts[2]);
    assert!(verifier(&src).verify(&tampered).await.is_err());
}

#[tokio::test]
async fn jwks_cached_then_refreshed_on_unknown_kid() {
    let src = StubSource::new(&[key_a()]);
    let v = verifier(&src).with_timing(DEFAULT_JWKS_TTL, Duration::ZERO);
    let tok_a = sign(key_a(), Some("key-a"), &good_claims());
    v.verify(&tok_a).await.unwrap();
    v.verify(&tok_a).await.unwrap();
    // metadata + jwks once; second verify served from cache.
    assert_eq!(src.fetches.load(Ordering::SeqCst), 2);

    // Key rotation: key-b appears upstream, token for it forces one refresh.
    *src.jwks.lock().unwrap() = jwks_doc(&[key_a(), key_b()]);
    let tok_b = sign(key_b(), Some("key-b"), &good_claims());
    v.verify(&tok_b).await.expect("refresh on unknown kid");
    assert_eq!(src.fetches.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn unknown_kid_refresh_is_rate_limited() {
    let src = StubSource::new(&[key_a()]);
    let v = verifier(&src); // 60 s min refresh
    let bogus = sign(key_b(), Some("key-b"), &good_claims());
    assert!(v.verify(&bogus).await.is_err());
    let after_first = src.fetches.load(Ordering::SeqCst);
    for _ in 0..5 {
        assert!(v.verify(&bogus).await.is_err());
    }
    assert_eq!(src.fetches.load(Ordering::SeqCst), after_first);
}

#[tokio::test]
async fn jwks_refetched_after_ttl() {
    let src = StubSource::new(&[key_a()]);
    let v = verifier(&src).with_timing(Duration::from_millis(20), Duration::ZERO);
    let tok = sign(key_a(), Some("key-a"), &good_claims());
    v.verify(&tok).await.unwrap();
    tokio::time::sleep(Duration::from_millis(40)).await;
    v.verify(&tok).await.unwrap();
    assert_eq!(src.fetches.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn fetch_failure_and_empty_jwks_fail_closed() {
    struct Broken;
    #[async_trait]
    impl JwksSource for Broken {
        async fn get_json(&self, _: &str) -> Result<serde_json::Value, String> {
            Err("network down".into())
        }
    }
    let v = TeamsJwtVerifier::new(APP_ID, Box::new(Broken)).unwrap();
    let tok = sign(key_a(), Some("key-a"), &good_claims());
    assert!(v.verify(&tok).await.is_err());

    let src = StubSource::new(&[]);
    let err = verifier(&src).verify(&tok).await.unwrap_err();
    assert!(err.contains("no usable RSA keys"), "{err}");
}
