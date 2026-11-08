//! One RustFS instance per client account (ADR-126 S5), enforced.
//!
//! The rule is the isolation: an account's bytes live on an instance only that account's user
//! runs, with credentials only that user holds. Two accounts on one instance would share the
//! credential, the data directory and the IAM table (and a principal's RustFS key has a single
//! policy, so two services granting the same principal on one instance overwrite each other).
//! The listener and credential-file checks in `open_rustfs` catch a misconfigured endpoint; this
//! catches the rest: an instance carries a marker bucket naming its account, and a store that
//! opens on an instance marked for another account is refused with a message that says so.

use weftos_store::AdminBackend;
use weftos_store::BackendError;

/// The marker bucket every client instance carries. Its `client-account` tag names the owner.
pub const MARKER_BUCKET: &str = "weftos-instance";
const OWNER_TAG: &str = "client-account";

/// Mark `admin`'s instance as `account`'s, or confirm it already is. `place` names the instance
/// for the error (for example its endpoint). An instance marked for another account is an error,
/// whoever races whom: the first to create the marker owns the instance.
pub fn claim_instance(admin: &dyn AdminBackend, account: &str, place: &str) -> Result<(), String> {
    for _ in 0..3 {
        let buckets = admin.buckets().map_err(|e| format!("{place}: cannot read the instance marker: {e}"))?;
        match buckets.into_iter().find(|(n, _)| n == MARKER_BUCKET) {
            Some((_, tags)) => {
                let owner = tags.iter().find(|(k, _)| k == OWNER_TAG).map(|(_, v)| v.as_str());
                return match owner {
                    Some(o) if o == account => Ok(()),
                    Some(o) => Err(format!(
                        "{place} already belongs to client account '{o}'; account '{account}' needs its own RustFS instance \
                         (one instance per client account, ADR-126 S5)"
                    )),
                    None => Err(format!("{place} has a '{MARKER_BUCKET}' bucket with no owner tag; refusing to share it with account '{account}'")),
                };
            }
            None => {
                let tags = [(OWNER_TAG.to_owned(), account.to_owned()), ("rule".to_owned(), "one-instance-per-client-account".to_owned())];
                match admin.create_bucket(MARKER_BUCKET, &tags) {
                    Ok(()) => return Ok(()),
                    // Someone else created it between our read and our write: read it again.
                    Err(BackendError::BucketExists(_)) => {}
                    Err(e) => return Err(format!("{place}: cannot mark the instance: {e}")),
                }
            }
        }
    }
    Err(format!("{place}: the instance marker kept changing; try again"))
}
