//! Who may hold a display. ADR-117 section 5 puts this on ADR-108
//! per-resource grants; until those exist for displays the endpoint asks
//! a [`GrantPolicy`], so the check has one home and the RPC layer never
//! re-implements it. The default policy admits any subject the RPC layer
//! already authorised (`Write`) and lets an operator revoke a subject,
//! which ends that subject's session on its next op.

use std::collections::HashSet;
use std::sync::Mutex;

/// What an app presents to open a session.
#[derive(Debug, Clone)]
pub struct GrantRequest {
    pub subject: String,
    /// The display resource, `weftos://<mesh>/nodes/<leaf>/services/display`
    /// or just the leaf id.
    pub resource: String,
    pub leaf_id: String,
    /// Opaque proof (an ADR-108 chain in D2). Carried to the leaf, not
    /// yet verified.
    pub proof: Vec<u8>,
}

pub trait GrantPolicy: Send + Sync {
    /// May this subject open a session now?
    fn admit(&self, req: &GrantRequest) -> Result<(), String>;
    /// Is the subject still allowed? Asked on every op and poll.
    fn still_valid(&self, subject: &str, leaf_id: &str) -> bool;
}

/// Admit everyone the RPC layer let through; honour revocations.
#[derive(Default)]
pub struct RevocablePolicy {
    revoked: Mutex<HashSet<String>>,
}

impl RevocablePolicy {
    pub fn revoke(&self, subject: &str) {
        self.revoked.lock().unwrap_or_else(|e| e.into_inner()).insert(subject.to_owned());
    }

    /// Lift a revocation (a new grant for the subject).
    pub fn restore(&self, subject: &str) {
        self.revoked.lock().unwrap_or_else(|e| e.into_inner()).remove(subject);
    }

    fn is_revoked(&self, subject: &str) -> bool {
        self.revoked.lock().unwrap_or_else(|e| e.into_inner()).contains(subject)
    }
}

impl GrantPolicy for RevocablePolicy {
    fn admit(&self, req: &GrantRequest) -> Result<(), String> {
        if req.subject.trim().is_empty() {
            return Err("a display grant needs a subject".into());
        }
        if self.is_revoked(&req.subject) {
            return Err(format!("grant for {} was revoked", req.subject));
        }
        Ok(())
    }

    fn still_valid(&self, subject: &str, _leaf_id: &str) -> bool {
        !self.is_revoked(subject)
    }
}
