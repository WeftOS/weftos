//! Card 10a2eb4a: bearer token ids must not reach logs or chain events,
//! and `new(key)` must use the caller's key.
//!
//! A `token_id` is the bearer handle that `validate_token`,
//! `validate_auth_token` and `check_scope` accept, so anyone who can read
//! the logs or the chain could use a live token. Logs and chain payloads
//! carry `token_fp` instead: the first 8 bytes of SHA-256(token_id), hex.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use super::*;

/// Minimal subscriber that renders every event's fields as `name=value;`.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Render<'a>(&'a mut String);

impl Visit for Render<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={:?};", field.name(), value);
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, "{}={};", field.name(), value);
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut line = String::new();
        event.record(&mut Render(&mut line));
        self.0.lock().unwrap().push(line);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

fn expected_fp(token_id: &str) -> String {
    let digest = Sha256::digest(token_id.as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

fn request(cred: &str) -> CredentialRequest {
    CredentialRequest {
        credential_name: cred.to_string(),
        requester_pid: 1,
        agent_id: "agent".to_string(),
        scope: vec!["read".to_string()],
        ttl_secs: 60,
    }
}

#[test]
fn issued_token_id_is_not_logged() {
    let cap = Capture::default();
    let svc = AuthService::new([0x42; 32]);
    svc.register_credential("c", CredentialType::ApiKey, b"v", vec![])
        .unwrap();
    let token = tracing::subscriber::with_default(cap.clone(), || {
        svc.request_token(&request("c")).unwrap()
    });
    let lines = cap.0.lock().unwrap().clone();
    let issued: Vec<&String> = lines.iter().filter(|l| l.contains("token issued")).collect();
    assert_eq!(issued.len(), 1, "expected one 'token issued' event, got {lines:?}");
    for line in &lines {
        assert!(
            !line.contains(&token.token_id),
            "a log event carries the bearer token id"
        );
    }
    assert!(
        issued[0].contains(&format!("token_fp={}", expected_fp(&token.token_id))),
        "the issue event should carry the token fingerprint"
    );
}

#[test]
fn auth_token_id_is_not_logged() {
    let cap = Capture::default();
    let svc = AuthService::new([0x42; 32]);
    svc.register_hashed_credential("agent-1", b"raw", vec!["read".into()])
        .unwrap();
    let token = tracing::subscriber::with_default(cap.clone(), || {
        svc.authenticate("agent-1", b"raw").unwrap()
    });
    let lines = cap.0.lock().unwrap().clone();
    assert!(lines.iter().any(|l| l.contains("agent authenticated")), "{lines:?}");
    for line in &lines {
        assert!(!line.contains(&token.token_id), "a log event carries the auth token id");
    }
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("token_fp={}", expected_fp(&token.token_id))))
    );
}

#[cfg(feature = "exochain")]
#[test]
fn token_ids_are_not_written_to_the_chain() {
    let cm = Arc::new(crate::chain::ChainManager::new(0, 1000));
    let svc = AuthService::new([0x42; 32]).with_chain_manager(cm.clone());
    svc.register_credential("c", CredentialType::ApiKey, b"v", vec![])
        .unwrap();
    svc.register_hashed_credential("agent-1", b"raw", vec![])
        .unwrap();
    let issued = svc.request_token(&request("c")).unwrap();
    let auth = svc.authenticate("agent-1", b"raw").unwrap();
    assert!(svc.revoke_token(&issued.token_id).unwrap());
    let events = cm.tail(100);
    let auth_events: Vec<_> = events.iter().filter(|e| e.source == "auth").collect();
    assert!(auth_events.len() >= 3, "expected issue, attempt and revoke events");
    for e in &auth_events {
        let payload = e.payload.as_ref().map(|p| p.to_string()).unwrap_or_default();
        assert!(!payload.contains(&issued.token_id), "{} carries a token id", e.kind);
        assert!(!payload.contains(&auth.token_id), "{} carries a token id", e.kind);
    }
    let fp = expected_fp(&issued.token_id);
    for kind in [
        crate::chain::EVENT_KIND_AUTH_TOKEN_ISSUE,
        crate::chain::EVENT_KIND_AUTH_TOKEN_REVOKE,
    ] {
        let e = auth_events.iter().find(|e| e.kind == kind).expect(kind);
        assert_eq!(e.payload.as_ref().unwrap()["token_fp"], fp, "{kind}");
    }
}

// ── item 3: new(key) uses the caller's key ─────────────────────────────

#[test]
fn same_key_opens_credentials_across_instances() {
    let a = AuthService::new([0x5A; 32]);
    let b = AuthService::new([0x5A; 32]);
    a.register_credential("c", CredentialType::ApiKey, b"shared", vec![])
        .unwrap();
    b.register_credential("c", CredentialType::ApiKey, b"placeholder", vec![])
        .unwrap();
    let sealed = a.credentials.get("c").unwrap().encrypted_value.clone();
    b.credentials.get_mut("c").unwrap().encrypted_value = sealed;
    assert_eq!(&b.open_credential("c").unwrap()[..], b"shared");
}
