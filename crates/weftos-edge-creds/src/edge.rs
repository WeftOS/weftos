//! Edge / Wasm side: reads and writes the node's own bucket with credentials
//! it got from its parent a moment ago. Holds no key at construction, keeps
//! the session in memory only, and refuses anything that is not short-lived.

use std::sync::Mutex;

use crate::clock::Clock;
use crate::creds::{CredentialRefusal, TempCredentials};
use crate::policy::{NameError, validate_key};
use crate::sigv4::HeaderSigner;
use crate::wire::{
    Endpoint, HttpRequest, HttpResponse, LeaseRefused, LeaseRequest, LeaseResponse, ObjectTransport,
    ParentLink, TransportError,
};

#[derive(Debug, thiserror::Error)]
pub enum EdgeError {
    #[error("parent: {0}")]
    Parent(#[from] LeaseRefused),
    #[error("credential: {0}")]
    Credential(#[from] CredentialRefusal),
    #[error("{0}")]
    Name(#[from] NameError),
    #[error("{0}")]
    Transport(#[from] TransportError),
    #[error("parent returned the wrong lease kind")]
    UnexpectedLease,
    #[error("store refused the request (HTTP {0})")]
    Forbidden(u16),
    #[error("store error (HTTP {0})")]
    Status(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    Written,
    /// `If-None-Match: *` hit an existing key (HTTP 412).
    AlreadyExists,
}

struct Lease {
    creds: TempCredentials,
    bucket: String,
    region: String,
    endpoint: Endpoint,
}

pub struct EdgeNodeStore<L, T, C> {
    link: L,
    transport: T,
    clock: C,
    lease: Mutex<Option<Lease>>,
}

impl<L: ParentLink, T: ObjectTransport, C: Clock> EdgeNodeStore<L, T, C> {
    /// No key goes in: the only secret source is the parent link.
    pub fn new(link: L, transport: T, clock: C) -> Self {
        Self { link, transport, clock, lease: Mutex::new(None) }
    }

    /// Drop the cached session (shutdown, or a lease the store refused).
    pub fn forget(&self) {
        *self.lease.lock().expect("lease") = None;
    }

    async fn fresh_lease(&self) -> Result<(), EdgeError> {
        let resp = self.link.request_lease(LeaseRequest::Sts).await?;
        let LeaseResponse::Sts { credentials, bucket, region, endpoint } = resp else {
            return Err(EdgeError::UnexpectedLease);
        };
        credentials.admit(self.clock.now())?;
        *self.lease.lock().expect("lease") = Some(Lease { creds: credentials, bucket, region, endpoint });
        Ok(())
    }

    async fn ensure_lease(&self) -> Result<(), EdgeError> {
        let now = self.clock.now();
        let stale = match &*self.lease.lock().expect("lease") {
            Some(l) => l.creds.needs_refresh(now),
            None => true,
        };
        if stale { self.fresh_lease().await } else { Ok(()) }
    }

    fn build(&self, method: &str, key: &str, extra: &[(String, String)], body: &[u8]) -> HttpRequest {
        let guard = self.lease.lock().expect("lease");
        let l = guard.as_ref().expect("lease ensured");
        let path = format!("/{}/{key}", l.bucket);
        let base = vec![("host".to_string(), l.endpoint.host.clone())];
        let mut base = base;
        base.extend(extra.iter().cloned());
        let signer = HeaderSigner {
            access_key: &l.creds.access_key,
            secret_key: &l.creds.secret_key,
            session_token: Some(&l.creds.session_token),
            region: &l.region,
            now: self.clock.now(),
        };
        let headers = signer.sign(method, &path, &[], &base, body);
        HttpRequest {
            method: method.into(),
            scheme: l.endpoint.scheme.clone(),
            host: l.endpoint.host.clone(),
            path,
            query: vec![],
            headers,
            body: body.to_vec(),
        }
    }

    /// Sign and send; on a 403 get a fresh lease once (expiry, or a session
    /// the parent has since replaced). A second 403 is final: a revoked node
    /// is also refused by the parent, which surfaces as `EdgeError::Parent`.
    async fn exec(&self, method: &str, key: &str, extra: &[(String, String)], body: &[u8]) -> Result<HttpResponse, EdgeError> {
        validate_key(key)?;
        for attempt in 0..2 {
            self.ensure_lease().await?;
            let resp = self.transport.send(self.build(method, key, extra, body)).await?;
            if resp.status == 403 {
                self.forget();
                if attempt == 0 {
                    continue;
                }
                return Err(EdgeError::Forbidden(403));
            }
            return Ok(resp);
        }
        Err(EdgeError::Forbidden(403))
    }

    pub async fn put(&self, key: &str, body: &[u8]) -> Result<(), EdgeError> {
        let r = self.exec("PUT", key, &[], body).await?;
        if (200..300).contains(&r.status) { Ok(()) } else { Err(EdgeError::Status(r.status)) }
    }

    /// Conditional create (S0: `If-None-Match: *` gives 412 on an existing key).
    pub async fn put_if_absent(&self, key: &str, body: &[u8]) -> Result<PutOutcome, EdgeError> {
        let r = self.exec("PUT", key, &[("if-none-match".into(), "*".into())], body).await?;
        match r.status {
            200..=299 => Ok(PutOutcome::Written),
            412 => Ok(PutOutcome::AlreadyExists),
            s => Err(EdgeError::Status(s)),
        }
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, EdgeError> {
        let r = self.exec("GET", key, &[], &[]).await?;
        match r.status {
            200..=299 => Ok(Some(r.body)),
            404 => Ok(None),
            s => Err(EdgeError::Status(s)),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<(), EdgeError> {
        let r = self.exec("DELETE", key, &[], &[]).await?;
        if (200..300).contains(&r.status) || r.status == 404 { Ok(()) } else { Err(EdgeError::Status(r.status)) }
    }

    /// The credential-less path: the parent presigns one PUT for this key and
    /// the edge sends it with nothing but `Host`.
    pub async fn put_presigned(&self, key: &str, body: &[u8], ttl_secs: u64) -> Result<(), EdgeError> {
        validate_key(key)?;
        let resp = self.link.request_lease(LeaseRequest::PresignPut { key: key.into(), ttl_secs }).await?;
        let LeaseResponse::Presigned(p) = resp else { return Err(EdgeError::UnexpectedLease) };
        if p.expires_at <= self.clock.now() {
            return Err(CredentialRefusal::Expired.into());
        }
        let req = HttpRequest {
            method: p.method,
            scheme: p.endpoint.scheme,
            host: p.endpoint.host.clone(),
            path: p.path,
            query: p.query,
            // Host only: RustFS rejects a presigned request carrying any x-amz-*
            // header that is not in the signed set (measured on 1.0.4).
            headers: vec![("host".into(), p.endpoint.host)],
            body: body.to_vec(),
        };
        let r = self.transport.send(req).await?;
        match r.status {
            200..=299 => Ok(()),
            403 => Err(EdgeError::Forbidden(403)),
            s => Err(EdgeError::Status(s)),
        }
    }
}
