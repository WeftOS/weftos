//! Parent-daemon side: turns an authenticated node into a scoped, short-lived
//! lease (ADR-126 section 5). The long-lived key for the node's RustFS user
//! lives behind [`StsMinter`] and never crosses the link.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::clock::Clock;
use crate::creds::{MAX_LEASE_TTL_SECS, TempCredentials};
use crate::policy::{
    Access, NameError, bucket_session_policy, node_bucket, object_session_policy, principal_for,
    validate_key,
};
use crate::sigv4::{self, HeaderSigner};
use crate::wire::{Endpoint, LeaseRefused, LeaseRequest, LeaseResponse, Presigned, RefusalCode};

#[derive(Debug, thiserror::Error)]
#[error("sts: {0}")]
pub struct StsError(pub String);

/// RustFS STS and IAM, as the daemon drives them (card b76c74a7 supplies the
/// real implementation; S1's control-plane client is the natural home).
pub trait StsMinter {
    /// AssumeRole as `principal` with an inline session `policy`.
    async fn assume_role(&self, principal: &str, policy: &str, ttl_secs: u64) -> Result<TempCredentials, StsError>;
    /// Disable the user: every session it issued is refused at the next request (S0: ~0.22 s).
    async fn disable_principal(&self, principal: &str) -> Result<(), StsError>;
    async fn enable_principal(&self, principal: &str) -> Result<(), StsError>;
}

/// The chain-recorded grant (ADR-126 section 3). A name grants nothing.
pub trait GrantCheck {
    fn allows(&self, node_id: &str, access: Access) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub at: u64,
    pub node_id: String,
    pub action: &'static str,
    /// `None` when issued, the refusal code otherwise.
    pub refused: Option<RefusalCode>,
    /// Never contains key material.
    pub detail: String,
}

/// Feeds the ADR-120 activity log.
pub trait AuditSink {
    fn record(&self, ev: AuditEvent);
}

#[derive(Debug, Clone)]
pub struct BrokerConfig {
    pub endpoint: Endpoint,
    pub region: String,
    pub sts_ttl_secs: u64,
    pub max_presign_secs: u64,
    /// At most `rate_max` leases per node per `rate_window_secs`.
    pub rate_max: usize,
    pub rate_window_secs: u64,
}

impl BrokerConfig {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            region: "us-east-1".into(),
            sts_ttl_secs: MAX_LEASE_TTL_SECS,
            max_presign_secs: MAX_LEASE_TTL_SECS,
            rate_max: 30,
            rate_window_secs: 60,
        }
    }
}

#[derive(Default)]
struct State {
    revoked: HashSet<String>,
    hits: HashMap<String, Vec<u64>>,
}

pub struct EdgeBroker<M, G, A, C> {
    cfg: BrokerConfig,
    minter: M,
    grants: G,
    audit: A,
    clock: C,
    state: Mutex<State>,
}

fn refuse(code: RefusalCode, message: &str) -> LeaseRefused {
    LeaseRefused { code, message: message.into() }
}

impl<M: StsMinter, G: GrantCheck, A: AuditSink, C: Clock> EdgeBroker<M, G, A, C> {
    pub fn new(cfg: BrokerConfig, minter: M, grants: G, audit: A, clock: C) -> Self {
        Self { cfg, minter, grants, audit, clock, state: Mutex::new(State::default()) }
    }

    /// The RustFS side, for pairing and unpairing (provision / deprovision).
    pub fn minter(&self) -> &M {
        &self.minter
    }

    fn note(&self, node: &str, action: &'static str, refused: Option<RefusalCode>, detail: String) {
        self.audit.record(AuditEvent { at: self.clock.now(), node_id: node.into(), action, refused, detail });
    }

    fn deny(&self, node: &str, action: &'static str, code: RefusalCode, msg: &str) -> LeaseRefused {
        self.note(node, action, Some(code), msg.into());
        refuse(code, msg)
    }

    fn admit(&self, node: &str, action: &'static str, access: Access) -> Result<(), LeaseRefused> {
        if crate::policy::validate_node_id(node).is_err() {
            return Err(self.deny(node, action, RefusalCode::BadRequest, "invalid node id"));
        }
        let now = self.clock.now();
        {
            let mut st = self.state.lock().expect("broker state");
            if st.revoked.contains(node) {
                drop(st);
                return Err(self.deny(node, action, RefusalCode::Revoked, "node access revoked"));
            }
            let hits = st.hits.entry(node.to_string()).or_default();
            hits.retain(|t| now.saturating_sub(*t) < self.cfg.rate_window_secs);
            if hits.len() >= self.cfg.rate_max {
                drop(st);
                return Err(self.deny(node, action, RefusalCode::RateLimited, "too many lease requests"));
            }
            hits.push(now);
        }
        if !self.grants.allows(node, access) {
            return Err(self.deny(node, action, RefusalCode::NotGranted, "no grant for this store"));
        }
        Ok(())
    }

    /// Serve one lease request from `node`, authenticated by the link. The
    /// bucket comes from `node`, never from the request.
    pub async fn lease(&self, node: &str, req: LeaseRequest) -> Result<LeaseResponse, LeaseRefused> {
        match req {
            LeaseRequest::Sts => self.sts(node).await,
            LeaseRequest::PresignPut { key, ttl_secs } => {
                self.presign(node, "presign_put", "PUT", "s3:PutObject", Access::Write, &key, ttl_secs).await
            }
            LeaseRequest::PresignGet { key, ttl_secs } => {
                self.presign(node, "presign_get", "GET", "s3:GetObject", Access::Read, &key, ttl_secs).await
            }
        }
    }

    async fn sts(&self, node: &str) -> Result<LeaseResponse, LeaseRefused> {
        self.admit(node, "sts", Access::Write)?;
        let bucket = node_bucket(node).map_err(|_| refuse(RefusalCode::BadRequest, "invalid node id"))?;
        let policy = bucket_session_policy(&bucket, Access::Write);
        let ttl = self.cfg.sts_ttl_secs.min(MAX_LEASE_TTL_SECS);
        let creds = self.minter.assume_role(&principal_for(node), &policy, ttl).await.map_err(|e| {
            self.deny(node, "sts", RefusalCode::Unavailable, &format!("minter: {}", e.0))
        })?;
        self.note(node, "sts", None, format!("bucket={bucket} ttl={ttl}s"));
        Ok(LeaseResponse::Sts {
            credentials: creds,
            bucket,
            region: self.cfg.region.clone(),
            endpoint: self.cfg.endpoint.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn presign(
        &self,
        node: &str,
        action: &'static str,
        method: &str,
        s3_action: &str,
        access: Access,
        key: &str,
        ttl_secs: u64,
    ) -> Result<LeaseResponse, LeaseRefused> {
        self.admit(node, action, access)?;
        if let Err(NameError::Key | NameError::NodeId) = validate_key(key) {
            return Err(self.deny(node, action, RefusalCode::BadRequest, "invalid object key"));
        }
        let bucket = node_bucket(node).map_err(|_| refuse(RefusalCode::BadRequest, "invalid node id"))?;
        let ttl = ttl_secs.clamp(1, self.cfg.max_presign_secs.min(MAX_LEASE_TTL_SECS));
        // The session behind the URL is scoped to this one object and action.
        let policy = object_session_policy(&bucket, key, s3_action);
        let creds = self.minter.assume_role(&principal_for(node), &policy, ttl.max(60)).await.map_err(|e| {
            self.deny(node, action, RefusalCode::Unavailable, &format!("minter: {}", e.0))
        })?;
        let now = self.clock.now();
        let signer = HeaderSigner {
            access_key: &creds.access_key,
            secret_key: &creds.secret_key,
            session_token: Some(&creds.session_token),
            region: &self.cfg.region,
            now,
        };
        let path = format!("/{bucket}/{key}");
        let p = sigv4::presign(&signer, &self.cfg.endpoint.host, method, &path, ttl);
        let url = sigv4::render_url(&self.cfg.endpoint.scheme, &self.cfg.endpoint.host, &p);
        self.note(node, action, None, format!("bucket={bucket} key_len={} ttl={ttl}s", key.len()));
        Ok(LeaseResponse::Presigned(Presigned {
            method: method.into(),
            endpoint: self.cfg.endpoint.clone(),
            path: p.path,
            query: p.query,
            url,
            expires_at: now + ttl,
        }))
    }

    /// Remove a node's access: no new leases, and the RustFS user is disabled
    /// so sessions already issued are refused at their next request.
    pub async fn revoke(&self, node: &str) -> Result<(), StsError> {
        self.state.lock().expect("broker state").revoked.insert(node.into());
        self.note(node, "revoke", None, String::new());
        self.minter.disable_principal(&principal_for(node)).await
    }

    pub async fn restore(&self, node: &str) -> Result<(), StsError> {
        self.minter.enable_principal(&principal_for(node)).await?;
        self.state.lock().expect("broker state").revoked.remove(node);
        self.note(node, "restore", None, String::new());
        Ok(())
    }
}
