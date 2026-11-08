//! How CI reaches a release bucket: one switch, two routes (ADR-126 S4).
//!
//! | Route | CI holds | Write-once is enforced by | Needs |
//! |---|---|---|---|
//! | [`CiRoute::Daemon`] | `append` on the store (read-only at RustFS) | the daemon: its `append` role refuses any put without `If-None-Match` | the weaver daemon in the path (`svc.store`) |
//! | [`CiRoute::Direct`] | its own RustFS key ([`direct_ci_policy`](crate::s3::direct_ci_policy)) | the publisher's own `If-None-Match` only; the key can replace what it can write | S3 reachable from CI, nothing else |
//!
//! The owner's pick is the one line [`DEFAULT_CI_ROUTE`]. Everything that builds a CI backend calls
//! [`CiRoute::default`] or takes the route as a parameter, so no caller changes when it flips.
//!
//! Both routes hand the publisher the same [`ReleaseBackend`]; [`CiBackend`] is the enum over them.
//! The sweeper's grant is separate (see [`ServiceBackend::copy`]).

use std::sync::Arc;

use weftos_store::service::Caller;
use weftos_store::{ObjectBackend, StoreError, StoreName, StoreService};
use weftos_store::backend::{BackendError as StoreErr, PutOpts, Retention};

use crate::backend::{BackendError, DeletableBackend, KeyVersion, Lock, LockMode, PutOutcome, ReleaseBackend};
use crate::name::ReleaseStoreName;
use crate::s3::StoreBackend;

/// **The switch.** Flip this line to change the route every default caller uses.
///
/// `Direct` is today's behaviour and the only route a standalone CLI can run (`weaver store release
/// publish` has no daemon client yet). Flip to `Daemon` when the owner picks it and the publisher runs
/// inside the weaver daemon; the verb then refuses with a message that says so instead of falling back.
pub const DEFAULT_CI_ROUTE: CiRoute = CiRoute::Direct;

/// Which way CI reaches a release bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiRoute {
    /// Through the weaver daemon's store service, as an `append` principal.
    Daemon,
    /// Straight to RustFS with the CI key's own policy.
    Direct,
}

impl Default for CiRoute {
    fn default() -> Self {
        DEFAULT_CI_ROUTE
    }
}

impl CiRoute {
    /// `daemon` or `direct`.
    pub fn parse(s: &str) -> Result<Self, BackendError> {
        match s {
            "daemon" => Ok(Self::Daemon),
            "direct" => Ok(Self::Direct),
            o => Err(BackendError(format!("unknown CI route {o:?} (daemon, direct)"))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Direct => "direct",
        }
    }
}

fn store_err(e: StoreError) -> BackendError {
    BackendError(e.to_string())
}

/// A release store reached through the daemon's [`StoreService`] as one principal. Every call is
/// gated by that principal's grant and written to the store's activity log, so an `append` CI
/// principal is held to write-once by the service, not just by the publisher.
pub struct ServiceBackend {
    svc: Arc<StoreService>,
    who: Caller,
    name: StoreName,
}

impl ServiceBackend {
    pub fn new(svc: Arc<StoreService>, who: Caller, store: &ReleaseStoreName) -> Result<Self, BackendError> {
        let name = StoreName::parse(&store.uri()).map_err(|e| BackendError(e.to_string()))?;
        Ok(Self { svc, who, name })
    }

    /// Run a blocking service call off the async threads (the RustFS transport blocks).
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&StoreService, &Caller, &StoreName) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let (svc, who, name) = (self.svc.clone(), self.who.clone(), self.name.clone());
        tokio::task::spawn_blocking(move || f(&svc, &who, &name))
            .await
            .map_err(|e| StoreError::Backend(StoreErr::Io(format!("blocking task: {e}"))))?
    }
}

impl ReleaseBackend for ServiceBackend {
    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, BackendError> {
        let (k, d) = (key.to_string(), bytes.to_vec());
        match self.blocking(move |s, w, n| s.put(w, n, &k, &d, true)).await {
            Ok(_) => Ok(PutOutcome::Created),
            Err(StoreError::Backend(StoreErr::Exists(_))) => Ok(PutOutcome::Exists),
            Err(e) => Err(store_err(e)),
        }
    }

    async fn put_if_absent_locked(&self, key: &str, bytes: &[u8], lock: &Lock) -> Result<PutOutcome, BackendError> {
        let (k, d) = (key.to_string(), bytes.to_vec());
        let opts = PutOpts {
            if_none_match: true,
            retention: Some(Retention {
                mode: match lock.mode {
                    LockMode::Governance => weftos_store::LockMode::Governance,
                    LockMode::Compliance => weftos_store::LockMode::Compliance,
                },
                until_unix_s: lock.until,
            }),
            legal_hold: false,
        };
        match self.blocking(move |s, w, n| s.put_locked(w, n, &k, &d, &opts)).await {
            Ok(_) => Ok(PutOutcome::Created),
            Err(StoreError::Backend(StoreErr::Exists(_))) => Ok(PutOutcome::Exists),
            Err(e) => Err(store_err(e)),
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BackendError> {
        let k = key.to_string();
        match self.blocking(move |s, w, n| s.get(w, n, &k)).await {
            Ok(v) => Ok(Some(v)),
            Err(StoreError::Backend(StoreErr::NoObject(_))) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, BackendError> {
        let p = prefix.to_string();
        let metas = self.blocking(move |s, w, n| s.list(w, n, &p)).await.map_err(store_err)?;
        Ok(metas.into_iter().map(|m| m.key).collect())
    }

    async fn versions(&self, prefix: &str) -> Result<Option<Vec<KeyVersion>>, BackendError> {
        let p = prefix.to_string();
        match self.blocking(move |s, w, n| s.list_versions(w, n, &p)).await {
            Ok(vs) => Ok(Some(
                vs.into_iter()
                    .map(|v| KeyVersion {
                        key: v.key,
                        version_id: v.version_id,
                        is_latest: v.is_latest,
                        is_delete_marker: v.is_delete_marker,
                        etag: v.etag,
                    })
                    .collect(),
            )),
            Err(StoreError::Backend(StoreErr::Unsupported(_))) => Ok(None),
            Err(e) => Err(store_err(e)),
        }
    }
}

/// The sweeper's side of the service route. **A sweeper principal needs more than S1's `sweep` role.**
/// `sweep` is read plus delete, with no put and no copy; the quarantine step parks a blob by copying it
/// and a locked restore puts it back, so under `sweep` the first blob deletion is refused (and nothing is
/// deleted: the refusal comes before the delete). The service has no copy call, so `copy` here is a get
/// then an unconditional put and needs `write`. That is the open "sweeper put" grant.
impl DeletableBackend for ServiceBackend {
    async fn delete(&self, key: &str) -> Result<(), BackendError> {
        let k = key.to_string();
        match self.blocking(move |s, w, n| s.delete(w, n, &k)).await {
            Ok(()) | Err(StoreError::Backend(StoreErr::NoObject(_))) => Ok(()),
            Err(e) => Err(store_err(e)),
        }
    }

    async fn copy(&self, src: &str, dst: &str) -> Result<(), BackendError> {
        let (s_key, d_key) = (src.to_string(), dst.to_string());
        self.blocking(move |s, w, n| {
            let bytes = s.get(w, n, &s_key)?;
            s.put(w, n, &d_key, &bytes, false).map(|_| ())
        })
        .await
        .map_err(store_err)
    }
}

/// CI's backend on whichever route was chosen. `O` is the S3 transport used by [`CiRoute::Direct`].
pub enum CiBackend<O: ObjectBackend + 'static> {
    Daemon(ServiceBackend),
    Direct(StoreBackend<O>),
}

impl<O: ObjectBackend + 'static> CiBackend<O> {
    /// Which route this is.
    pub fn route(&self) -> CiRoute {
        match self {
            Self::Daemon(_) => CiRoute::Daemon,
            Self::Direct(_) => CiRoute::Direct,
        }
    }
}

macro_rules! either {
    ($self:ident, $b:ident => $e:expr) => {
        match $self {
            CiBackend::Daemon($b) => $e,
            CiBackend::Direct($b) => $e,
        }
    };
}

impl<O: ObjectBackend + 'static> ReleaseBackend for CiBackend<O> {
    async fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, BackendError> {
        either!(self, b => b.put_if_absent(key, bytes).await)
    }
    async fn put_if_absent_locked(&self, key: &str, bytes: &[u8], lock: &Lock) -> Result<PutOutcome, BackendError> {
        either!(self, b => b.put_if_absent_locked(key, bytes, lock).await)
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, BackendError> {
        either!(self, b => b.get(key).await)
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, BackendError> {
        either!(self, b => b.list(prefix).await)
    }
    async fn versions(&self, prefix: &str) -> Result<Option<Vec<KeyVersion>>, BackendError> {
        either!(self, b => b.versions(prefix).await)
    }
}

impl<O: ObjectBackend + 'static> DeletableBackend for CiBackend<O> {
    async fn delete(&self, key: &str) -> Result<(), BackendError> {
        either!(self, b => b.delete(key).await)
    }
    async fn copy(&self, src: &str, dst: &str) -> Result<(), BackendError> {
        either!(self, b => b.copy(src, dst).await)
    }
}

/// Build CI's backend for `route`. The daemon route needs the in-process service and the principal CI
/// acts as; the direct route needs the S3 transport. Passing the wrong half is refused here, once, so a
/// route flip cannot silently run with the other route's credentials.
pub fn ci_backend<O: ObjectBackend + 'static>(
    route: CiRoute,
    store: &ReleaseStoreName,
    daemon: Option<(Arc<StoreService>, Caller)>,
    direct: Option<Arc<O>>,
) -> Result<CiBackend<O>, BackendError> {
    match (route, daemon, direct) {
        (CiRoute::Daemon, Some((svc, who)), _) => Ok(CiBackend::Daemon(ServiceBackend::new(svc, who, store)?)),
        (CiRoute::Direct, _, Some(o)) => Ok(CiBackend::Direct(StoreBackend::for_release(o, store)?)),
        (CiRoute::Daemon, None, _) => Err(BackendError("CI route is daemon but no store service was given".into())),
        (CiRoute::Direct, _, None) => Err(BackendError("CI route is direct but no S3 transport was given".into())),
    }
}
