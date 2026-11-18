//! The user-signed forward header (ADR-103 A6, Phase 2 package I).
//!
//! When the user daemon proxies a call to a project's own kernel it stamps
//! `Request.forward = {project_id, issued_at_ms, sig}`, signed by the user key
//! (the chain key of Phase 1 D-1) over the newline-joined fields
//! `"weftos-project-forward-v2\n"`, project id, `issued_at_ms`, method,
//! SHA-256 hex of the canonical params JSON, and the target child key id.
//! The header therefore authorises exactly one request to exactly one child:
//! reattached to another method, other params, or presented to a sibling
//! child, it fails. The child verifies it against the `user_pubkey` of its own
//! project certificate:
//!
//! * the signature must be the user key's (domain-tagged distinctly from the
//!   cert, anchor, PoP and chain-event bytes, so one key never signs
//!   confusable bytes);
//! * `project_id` must be the child's bound project, and the signed target
//!   key id the child's own (its node id, which is its project key id);
//! * the method and params must be those of the request carrying the header;
//! * `issued_at_ms` must lie within [`FORWARD_WINDOW_MS`] of the child's
//!   clock, in either direction (5 s);
//! * single use: a signature is accepted once; entries are kept for twice the
//!   window on a monotonic clock, so a wall-clock step cannot reopen a replay.
//!
//! [`ForwardVerifier`] is process state: the child installs it once at boot
//! with the cert's user key ([`install_trust`]); without it every forward
//! header is refused (`forward_unavailable`), never ignored. Forward headers
//! are honoured only from local unix callers: the TCP relay strips them
//! (`relay_auth::sanitize_line`).
//!
//! Honest limit: the header proves "the holder of the user key forwarded this
//! one request for this project". It does not prove who asked the user
//! daemon; that is the user daemon's own authorisation. A same-uid process
//! that can read the user key can sign one too; this is an isolation guard
//! between one user's projects, not a boundary against a hostile local
//! process (Phase 3 peer credentials, Phase 4 sandboxes).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use clawft_rpc::ForwardHeader;
use clawft_types::project::canon::canonical_json;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::verified_project::VerifiedProject;

/// Domain tag of the signed bytes.
pub const FORWARD_DOMAIN: &str = "weftos-project-forward-v2\n";

/// Accepted clock skew / age of a forward header, in milliseconds.
pub const FORWARD_WINDOW_MS: u64 = 5_000;

/// Why a forward header was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardError {
    /// This process holds no user key to verify against (not a child, or not
    /// booted with a certificate).
    Unavailable,
    /// Malformed hex or wrong length.
    Malformed,
    /// The signature is not the user key's over the stated fields.
    BadSignature,
    /// The header names a project other than the one this kernel serves.
    WrongProject,
    /// The header was signed for another child (key id) than this one.
    /// Reserved kind: the target is inside the signed bytes, so today a
    /// mis-targeted header is reported as `forward_bad_signature`.
    WrongTarget,
    /// `issued_at_ms` is more than [`FORWARD_WINDOW_MS`] from now.
    OutsideWindow,
    /// The same signature was already accepted inside the window.
    Replayed,
}

impl ForwardError {
    /// `error_kind` for the refusal response.
    pub fn kind(self) -> &'static str {
        match self {
            Self::Unavailable => "forward_unavailable",
            Self::Malformed | Self::BadSignature => "forward_bad_signature",
            Self::WrongProject => "project_scope_mismatch",
            Self::WrongTarget => "forward_wrong_target",
            Self::OutsideWindow => "forward_expired",
            Self::Replayed => "forward_replayed",
        }
    }
}

impl std::fmt::Display for ForwardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let m = match self {
            Self::Unavailable => "this kernel has no user key to verify a forward header against",
            Self::Malformed => "forward header is malformed",
            Self::BadSignature => "forward header signature is not the user key's",
            Self::WrongProject => "forward header names a project other than this kernel's",
            Self::WrongTarget => "forward header was signed for another child kernel",
            Self::OutsideWindow => "forward header is outside its 5 s window",
            Self::Replayed => "forward header was already used",
        };
        f.write_str(m)
    }
}

impl std::error::Error for ForwardError {}

/// What a forward header is bound to besides its project and time: the
/// request it rides on and the child it is for.
#[derive(Debug, Clone, Copy)]
pub struct ForwardBinding<'a> {
    pub method: &'a str,
    pub params: &'a Value,
    /// The child's project key id (its node id).
    pub target_key_id: &'a str,
}

/// The signed bytes are newline-joined, so no field may contain a newline
/// (a method with one could shift bytes between fields).
fn fields_ok(project_id: &str, b: &ForwardBinding<'_>) -> bool {
    ![project_id, b.method, b.target_key_id].iter().any(|f| f.contains('\n'))
}

fn signed_bytes(project_id: &str, issued_at_ms: u64, b: &ForwardBinding<'_>) -> Vec<u8> {
    let params_hash = hex::encode(Sha256::digest(canonical_json(b.params).as_bytes()));
    format!(
        "{FORWARD_DOMAIN}{project_id}\n{issued_at_ms}\n{}\n{params_hash}\n{}",
        b.method, b.target_key_id
    )
    .into_bytes()
}

/// User-daemon side: sign a forward header for one proxied call.
pub fn sign_forward(
    user_key: &SigningKey,
    project_id: &str,
    issued_at_ms: u64,
    binding: &ForwardBinding<'_>,
) -> Result<ForwardHeader, ForwardError> {
    if !fields_ok(project_id, binding) {
        return Err(ForwardError::Malformed);
    }
    let sig = user_key.sign(&signed_bytes(project_id, issued_at_ms, binding));
    Ok(ForwardHeader {
        project_id: project_id.to_owned(),
        issued_at_ms,
        sig: hex::encode(sig.to_bytes()),
    })
}

/// User-daemon side: stamp `req` (a call proxied to `project_id`'s kernel
/// whose key id is `target_key_id`) with a fresh forward header bound to its
/// method and params. Also pins `req.project` to the same id.
///
/// The proxy must call this as its LAST step, after any rewriting of the
/// method or params: the header covers exactly the bytes it is given here,
/// so a later change makes the child refuse the call.
pub fn stamp_forward(
    req: &mut clawft_rpc::Request,
    user_key: &SigningKey,
    project_id: &str,
    target_key_id: &str,
    now_ms: u64,
) -> Result<(), ForwardError> {
    let binding = ForwardBinding {
        method: &req.method,
        params: &req.params,
        target_key_id,
    };
    let header = sign_forward(user_key, project_id, now_ms, &binding)?;
    req.project = Some(project_id.to_owned());
    req.forward = Some(header);
    Ok(())
}

/// Child side: verifies forward headers for one bound project.
pub struct ForwardVerifier {
    user_key: VerifyingKey,
    /// Signature -> when it was accepted (monotonic).
    seen: Mutex<HashMap<String, Instant>>,
}

impl ForwardVerifier {
    /// A verifier trusting `user_pubkey` (from the project certificate).
    pub fn new(user_pubkey: &[u8; 32]) -> Result<Self, ForwardError> {
        Ok(Self {
            user_key: VerifyingKey::from_bytes(user_pubkey).map_err(|_| ForwardError::Malformed)?,
            seen: Mutex::new(HashMap::new()),
        })
    }

    /// Verify `header` on the kernel bound to `bound_project`, for the
    /// request described by `binding`, at wall clock `now_ms` and monotonic
    /// time `now`.
    ///
    /// Order matters: the signature is checked before anything is recorded,
    /// so a forged header cannot fill the replay table. Entries are pruned
    /// once older than twice the window on the monotonic clock, so the table
    /// stays bounded by the rate of validly signed headers and a wall-clock
    /// step cannot make a spent signature acceptable again.
    pub fn verify(
        &self,
        header: &ForwardHeader,
        bound_project: &str,
        binding: &ForwardBinding<'_>,
        now_ms: u64,
        now: Instant,
    ) -> Result<VerifiedProject, ForwardError> {
        if !fields_ok(&header.project_id, binding) {
            return Err(ForwardError::Malformed);
        }
        let sig_bytes: [u8; 64] = hex::decode(&header.sig)
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(ForwardError::Malformed)?;
        self.user_key
            .verify(
                &signed_bytes(&header.project_id, header.issued_at_ms, binding),
                &Signature::from_bytes(&sig_bytes),
            )
            .map_err(|_| ForwardError::BadSignature)?;
        if header.project_id != bound_project {
            return Err(ForwardError::WrongProject);
        }
        if now_ms.abs_diff(header.issued_at_ms) > FORWARD_WINDOW_MS {
            return Err(ForwardError::OutsideWindow);
        }
        let mut seen = self.seen.lock().expect("seen lock poisoned");
        let keep = Duration::from_millis(2 * FORWARD_WINDOW_MS);
        seen.retain(|_, at| now.saturating_duration_since(*at) <= keep);
        match seen.entry(header.sig.clone()) {
            // A replay does not refresh the entry's age.
            std::collections::hash_map::Entry::Occupied(_) => return Err(ForwardError::Replayed),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(now);
            }
        }
        Ok(VerifiedProject::from_verified_forward(header.project_id.clone()))
    }
}

static TRUST: OnceLock<ForwardVerifier> = OnceLock::new();

/// Install the process's forward verifier (child boot, once). Returns
/// `false` if one is already installed.
pub fn install_trust(user_pubkey: &[u8; 32]) -> Result<bool, ForwardError> {
    Ok(TRUST.set(ForwardVerifier::new(user_pubkey)?).is_ok())
}

/// Verify against the installed trust at the wall clock. The target key id
/// is this kernel's own node id (the project key id of a child), taken from
/// its recorded instance attestation; without one the header is refused.
pub fn verify_installed(
    header: &ForwardHeader,
    bound_project: &str,
    method: &str,
    params: &Value,
) -> Result<VerifiedProject, ForwardError> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let verifier = TRUST.get().ok_or(ForwardError::Unavailable)?;
    let (_, own_key_id) =
        clawft_kernel::governance_project::instance_project().ok_or(ForwardError::Unavailable)?;
    let binding = ForwardBinding { method, params, target_key_id: own_key_id };
    verifier.verify(header, bound_project, &binding, now_ms, Instant::now())
}

#[cfg(test)]
#[path = "project_forward_tests.rs"]
mod tests;
