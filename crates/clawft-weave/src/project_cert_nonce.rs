//! Daemon-issued, single-use proof-of-possession nonces (see the parent
//! module docs).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use clawft_types::project::validate_id;

use super::IssueError;
use clawft_kernel::project_identity as ident;

/// How long an issued challenge nonce stays valid.
pub const CHALLENGE_TTL: Duration = Duration::from_secs(60);
const MAX_CHALLENGES: usize = 1024;
/// Outstanding challenges one project may hold; the oldest is dropped for a
/// new one, so a flood of `mesh.challenge` for one id cannot fill the table.
pub const MAX_PER_PROJECT: usize = 4;

/// A challenge nonce this daemon issued, not yet used. Only
/// [`claim_nonce`] makes one.
/// Deliberately not `Clone`: it is consumed by value.
#[derive(Debug, PartialEq, Eq)]
pub struct DaemonNonce {
    nonce: String,
    project_id: String,
}

impl DaemonNonce {
    /// The nonce, 32 lowercase hex.
    pub fn as_str(&self) -> &str {
        &self.nonce
    }
    /// The project it was issued for.
    pub fn project_id(&self) -> &str {
        &self.project_id
    }
}

static CHALLENGES: OnceLock<Mutex<HashMap<String, (String, Instant)>>> = OnceLock::new();

/// Issue a single-use challenge nonce for `project_id` (valid for
/// [`CHALLENGE_TTL`]; at most 1024 outstanding).
pub fn issue_challenge(project_id: &str) -> Result<String, IssueError> {
    validate_id(project_id).map_err(|_| IssueError::Invalid("project id is not a canonical ULID".into()))?;
    let mut raw = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut raw);
    let nonce = ident::hex(&raw);
    let mut map = CHALLENGES.get_or_init(Default::default).lock().expect("state lock poisoned");
    let now = Instant::now();
    map.retain(|_, (_, t)| *t > now);
    let mine: Vec<(String, Instant)> = map
        .iter()
        .filter(|(_, (p, _))| p == project_id)
        .map(|(n, (_, t))| (n.clone(), *t))
        .collect();
    if mine.len() >= MAX_PER_PROJECT
        && let Some((oldest, _)) = mine.into_iter().min_by_key(|(_, t)| *t)
    {
        map.remove(&oldest);
    }
    if map.len() >= MAX_CHALLENGES {
        return Err(IssueError::Unavailable("too many outstanding challenges".into()));
    }
    map.insert(nonce.clone(), (project_id.to_owned(), now + CHALLENGE_TTL));
    Ok(nonce)
}

/// Consume a challenge: it must be one this daemon issued for `project_id`,
/// unexpired and unused. A claim for the wrong project fails without
/// burning the challenge (its real holder can still use it until it
/// expires); an expired one is dropped.
pub fn claim_nonce(nonce: &str, project_id: &str) -> Result<DaemonNonce, IssueError> {
    let fail = || IssueError::Pop("unknown, expired or already used challenge nonce".into());
    let mut map = CHALLENGES.get_or_init(Default::default).lock().expect("state lock poisoned");
    let Some((pid, exp)) = map.get(nonce) else { return Err(fail()) };
    if *exp <= Instant::now() {
        map.remove(nonce);
        return Err(fail());
    }
    if pid != project_id {
        return Err(fail());
    }
    let (pid, _) = map.remove(nonce).ok_or_else(fail)?;
    Ok(DaemonNonce { nonce: nonce.to_owned(), project_id: pid })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_project_cannot_hold_more_than_the_cap_and_others_are_unaffected() {
        let a = clawft_types::project::new_id();
        let b = clawft_types::project::new_id();
        let (a, b) = (a.as_str(), b.as_str());
        let other = issue_challenge(b).unwrap();
        let first = issue_challenge(a).unwrap();
        let mut last = first.clone();
        for _ in 0..MAX_PER_PROJECT + 2 {
            last = issue_challenge(a).unwrap();
        }
        assert!(claim_nonce(&first, a).is_err(), "the oldest was dropped");
        assert!(claim_nonce(&last, a).is_ok());
        assert!(claim_nonce(&other, b).is_ok(), "another project's challenge survives");
    }
}
