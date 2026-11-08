//! The single-writer lease on a view's work prefix: one small JSON object per `(bucket, prefix)`
//! under `.drive/leases/`, a name the drive never places or uploads (`check_rel` refuses a first
//! segment of `.drive`). Taken create-only; a stale one is taken over with a conditional put
//! against the etag just read; renewed and released the same way. Same skew rule as the S2 merge
//! claims: a lease stamped more than [`LEASE_SKEW_MS`] ahead of this host's clock is not live.

use serde::{Deserialize, Serialize};

use super::{LeaseInfo, Result, ViewError};
use crate::backend::{BackendError, ObjectBackend};
use crate::drive::cas::hash_of;

pub const LEASE_PREFIX: &str = ".drive/leases/";
pub const LEASE_SKEW_MS: u64 = crate::share::CLAIM_SKEW_MS;

/// What the lease object holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseBody {
    pub id: String,
    pub holder: String,
    pub bucket: String,
    pub prefix: String,
    pub acquired_ms: u64,
    pub expires_ms: u64,
}

/// A lease this view holds: the object key, the etag of our last write, and its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub key: String,
    pub etag: String,
    pub body: LeaseBody,
}

/// The lease key for a work prefix (already normalised).
pub fn lease_key(bucket: &str, prefix: &str) -> String {
    format!("{LEASE_PREFIX}{}.json", &hash_of(format!("{bucket}\0{prefix}").as_bytes())[..32])
}

pub(crate) fn info(l: &Lease) -> LeaseInfo {
    LeaseInfo { key: l.key.clone(), id: l.body.id.clone(), holder: l.body.holder.clone(), expires_ms: l.body.expires_ms }
}

/// Live: not expired, and not stamped from the future (a fast clock would hold it too long).
pub(crate) fn is_live(b: &LeaseBody, now: u64) -> bool {
    b.acquired_ms <= now.saturating_add(LEASE_SKEW_MS) && now < b.expires_ms
}

fn read<B: ObjectBackend + ?Sized>(b: &B, bucket: &str, key: &str) -> Result<Option<(String, Option<LeaseBody>)>> {
    let meta = match b.head(bucket, key) {
        Ok(m) => m,
        Err(BackendError::NoObject(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let body = match b.get(bucket, key) {
        Ok(d) => serde_json::from_slice(&d).ok(),
        Err(BackendError::NoObject(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(Some((meta.etag, body)))
}

/// Take the lease. Returns the previous holder's body when a stale lease was taken over.
pub(crate) fn acquire<B: ObjectBackend + ?Sized>(
    b: &B,
    bucket: &str,
    prefix: &str,
    holder: &str,
    ttl_ms: u64,
    now: u64,
) -> Result<(Lease, Option<LeaseBody>)> {
    let key = lease_key(bucket, prefix);
    let body = LeaseBody {
        id: ulid::Ulid::new().to_string().to_lowercase(),
        holder: holder.to_owned(),
        bucket: bucket.to_owned(),
        prefix: prefix.to_owned(),
        acquired_ms: now,
        expires_ms: now.saturating_add(ttl_ms),
    };
    let data = serde_json::to_vec(&body)?;
    match b.put(bucket, &key, &data, true) {
        Ok(m) => return Ok((Lease { key, etag: m.etag, body }, None)),
        Err(BackendError::Exists(_)) => {}
        Err(e) => return Err(e.into()),
    }
    // One is there. A body that does not parse names no holder and is treated as stale.
    let Some((etag, cur)) = read(b, bucket, &key)? else {
        // Gone between the put and the read: try create-only once more, never blind.
        let m = b.put(bucket, &key, &data, true).map_err(|e| match e {
            BackendError::Exists(_) => ViewError::LeaseHeld { holder: "unknown (raced)".into(), expires_ms: 0 },
            e => e.into(),
        })?;
        return Ok((Lease { key, etag: m.etag, body }, None));
    };
    if let Some(c) = cur.as_ref().filter(|c| is_live(c, now)) {
        return Err(ViewError::LeaseHeld { holder: c.holder.clone(), expires_ms: c.expires_ms });
    }
    match b.put_if_match(bucket, &key, &data, &etag) {
        Ok(m) => Ok((Lease { key, etag: m.etag, body }, cur)),
        Err(BackendError::PreconditionFailed(_) | BackendError::NoObject(_)) => {
            Err(ViewError::LeaseHeld { holder: "unknown (taken over by another opener first)".into(), expires_ms: 0 })
        }
        Err(BackendError::Unsupported) => Err(ViewError::LeaseTakeoverUnsupported),
        Err(e) => Err(e.into()),
    }
}

/// Is this lease still ours and unexpired? Called before every publish.
pub(crate) fn check<B: ObjectBackend + ?Sized>(b: &B, l: &Lease, now: u64) -> Result<()> {
    if now >= l.body.expires_ms {
        return Err(ViewError::LeaseExpired(l.body.expires_ms));
    }
    match b.head(&l.body.bucket, &l.key) {
        Ok(m) if m.etag == l.etag => Ok(()),
        Ok(_) => Err(ViewError::LeaseLost("the lease object was replaced by another holder".into())),
        Err(BackendError::NoObject(_)) => Err(ViewError::LeaseLost("the lease object is gone".into())),
        Err(e) => Err(e.into()),
    }
}

/// Extend the lease to `now + ttl_ms`, only if the object is still the one we wrote.
pub(crate) fn renew<B: ObjectBackend + ?Sized>(b: &B, l: &Lease, ttl_ms: u64, now: u64) -> Result<Lease> {
    let body = LeaseBody { expires_ms: now.saturating_add(ttl_ms), ..l.body.clone() };
    let data = serde_json::to_vec(&body)?;
    let lost = |why: &str| ViewError::LeaseLost(why.to_owned());
    let m = match b.put_if_match(&l.body.bucket, &l.key, &data, &l.etag) {
        Ok(m) => m,
        Err(BackendError::PreconditionFailed(_)) => return Err(lost("replaced by another holder")),
        Err(BackendError::NoObject(_)) => return Err(lost("the lease object is gone")),
        Err(BackendError::Unsupported) => {
            // Head-then-write: not atomic, the window is stated in the spec.
            match b.head(&l.body.bucket, &l.key) {
                Ok(m) if m.etag == l.etag => b.put(&l.body.bucket, &l.key, &data, false)?,
                Ok(_) => return Err(lost("replaced by another holder")),
                Err(BackendError::NoObject(_)) => return Err(lost("the lease object is gone")),
                Err(e) => return Err(e.into()),
            }
        }
        Err(e) => return Err(e.into()),
    };
    Ok(Lease { key: l.key.clone(), etag: m.etag, body })
}

/// Delete the lease if it is still ours. `Ok(false)`: it was not (taken over, or already gone).
pub(crate) fn release<B: ObjectBackend + ?Sized>(b: &B, l: &Lease) -> Result<bool> {
    match b.delete_if_match(&l.body.bucket, &l.key, &l.etag) {
        Ok(()) => Ok(true),
        Err(BackendError::PreconditionFailed(_) | BackendError::NoObject(_)) => Ok(false),
        Err(BackendError::Unsupported) => match b.head(&l.body.bucket, &l.key) {
            Ok(m) if m.etag == l.etag => {
                b.delete(&l.body.bucket, &l.key)?;
                Ok(true)
            }
            Ok(_) | Err(BackendError::NoObject(_)) => Ok(false),
            Err(e) => Err(e.into()),
        },
        Err(e) => Err(e.into()),
    }
}
