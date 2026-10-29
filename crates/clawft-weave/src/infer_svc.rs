//! The proxy's session on the cog data plane (ADR-101 update 2026-10-28):
//! how an `infer.<role>` whose backend is a cogbus service (`svc.infer/<node>`,
//! the Venq's NPU cog) is called.
//!
//! The daemon owns the broker in-process, so the proxy registers one instance
//! of its own ([`INSTANCE`]) and opens a session on it without a socket. That
//! instance is a normal broker principal: it declares `svc.infer` as an input
//! and a blob output, every call is governed (`cogbus.call`,
//! `cogbus.blob.put`), counted in the bus activity under the proxy's
//! identity, and subject to the broker's per-caller pending cap. Which client
//! asked (`local:<addr>` or `mesh:<node>`) goes to the proxy's audit trail
//! with each call, not into the bus.
//!
//! An image is put as a blob owned by the proxy with a short TTL: a service
//! call body is capped at 64 KiB, an image is not. The handle rides in the
//! call body (`image_blob`), which is what lets the provider read it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clawft_kernel::infer_proxy::{ProxyError, SvcBackend, SvcImage, SvcRoute};
use clawft_kernel::workload_pkg::manifest::valid_token;
use clawft_types::cog_topics::{InputPolicy, TopicClass};
use serde_json::Value;
use tokio::sync::oneshot;
use weftos_cogbus::{Broker, ErrorCode, Event, InputSpec, InstanceSpec, OutputSpec, Session};

/// The proxy's instance id on the broker.
pub const INSTANCE: &str = "infer-proxy";
/// Its cog id.
pub const COG: &str = "weft-infer-proxy";
/// The service kind a svc role may call.
pub const KIND: &str = "svc.infer";
/// Blob kind the proxy declares for the images it uploads.
const BLOB_KIND: &str = "infer.image";
/// An uploaded image is kept this long; the provider reads it within the call.
const IMAGE_TTL_MS: u64 = 5 * 60 * 1000;
/// Longest wait for a reply (vision prompts on an NPU take seconds).
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Raw bytes per blob slice (the broker's chunk size).
const CHUNK: usize = 32 * 1024;

/// A role backed by a service on this node's cog data plane.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SvcCfg {
    /// `svc.infer/<source_id>`, e.g. `svc.infer/venq-npu`.
    pub topic: String,
    /// Model id of the service's catalog (`qwen3-vl-4b`).
    pub model: String,
    /// Project the proxy's bus identity belongs to, when the provider is in
    /// one and bus policy grants calls across projects to the proxy.
    #[serde(default)]
    pub project: Option<String>,
}

impl SvcCfg {
    /// Boundary validation.
    pub fn validate(&self, role: &str) -> Result<(), String> {
        let src = self.topic.strip_prefix("svc.infer/").unwrap_or("");
        let src_ok = !src.is_empty()
            && src.len() <= 64
            && src.as_bytes()[0].is_ascii_alphanumeric()
            && src.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c));
        if !src_ok {
            return Err(format!("role {role}: svc.topic is svc.infer/<source_id> (lower-case letters, digits, . _ -)"));
        }
        if !valid_token(&self.model, 128) {
            return Err(format!("role {role}: svc.model is 1..=128 characters of a model id"));
        }
        if self.project.as_deref().is_some_and(|p| !valid_token(p, 64)) {
            return Err(format!("role {role}: bad svc.project"));
        }
        Ok(())
    }
}

type Waiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Event>>>>;

/// A session of the proxy instance, calling services in-process.
pub struct BrokerSvc {
    broker: Arc<Broker>,
    session: Session,
    next: AtomicU64,
    waiters: Waiters,
    _pump: tokio::task::JoinHandle<()>,
}

impl BrokerSvc {
    /// Register the proxy instance on `broker` (in `project`, if the policy
    /// grants calls across projects to it) and open its session. Needs a
    /// running tokio runtime.
    pub fn open(broker: Arc<Broker>, project: Option<String>) -> Result<Arc<Self>, String> {
        let spec = InstanceSpec {
            instance_id: INSTANCE.into(),
            cog_id: COG.into(),
            project_id: project,
            uid: None,
            outputs: vec![OutputSpec { topic: BLOB_KIND.into(), schema: "blob.v1".into(), class: TopicClass::Blob }],
            inputs: vec![InputSpec { topic: KIND.into(), schema: "infer.v1".into(), policy: InputPolicy::default() }],
        };
        // The token is never given to anyone: the session is opened here.
        let token: String = rand::random::<[u8; 32]>().iter().map(|b| format!("{b:02x}")).collect();
        broker.register(spec, &token).map_err(|e| format!("register {INSTANCE}: {}", e.message))?;
        let mut session = broker.authenticate(&token, None).map_err(|e| format!("open {INSTANCE}: {}", e.message))?;
        let mut rx = session.take_pushed().ok_or("session has no push channel")?;
        let waiters: Waiters = Arc::default();
        let w = waiters.clone();
        let pump = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let id = match &ev {
                    Event::Ok { id, .. } | Event::Error { id, .. } => *id,
                    _ => continue,
                };
                if let Some(tx) = w.lock().unwrap_or_else(|p| p.into_inner()).remove(&id) {
                    let _ = tx.send(ev);
                }
            }
        });
        Ok(Arc::new(Self { broker, session, next: AtomicU64::new(1), waiters, _pump: pump }))
    }

    /// Put `image` as a blob and return its handle.
    fn put_image(&self, image: &SvcImage) -> Result<Value, ProxyError> {
        let b = &self.broker;
        let began = b
            .blob_begin(&self.session, image.media_type, image.bytes.len() as u64, Some(IMAGE_TTL_MS))
            .map_err(|e| map_err(e.code, &e.message))?;
        let upload = began["upload"].as_u64().ok_or_else(|| ProxyError::Upstream("blob_begin gave no upload id".into()))?;
        let total = image.bytes.len();
        let mut offset = 0usize;
        for piece in image.bytes.chunks(CHUNK) {
            offset += piece.len();
            let last = offset == total;
            let r = b
                .blob_chunk(&self.session, upload, (offset - piece.len()) as u64, &B64.encode(piece), last)
                .map_err(|e| map_err(e.code, &e.message))?;
            if last {
                return r.get("handle").cloned().ok_or_else(|| ProxyError::Upstream("blob upload gave no handle".into()));
            }
        }
        Err(ProxyError::BadRequest("empty image".into()))
    }
}

/// A bus refusal as the proxy's error.
fn map_err(code: ErrorCode, message: &str) -> ProxyError {
    match code {
        ErrorCode::Unavailable => ProxyError::NoInstance(message.to_string()),
        ErrorCode::Denied | ErrorCode::CrossProject | ErrorCode::Revoked | ErrorCode::Unauthorized => {
            ProxyError::Refused(message.to_string())
        }
        ErrorCode::TooLarge => ProxyError::TooLarge(message.to_string()),
        ErrorCode::Timeout => ProxyError::Timeout(message.to_string()),
        _ => ProxyError::Upstream(message.to_string()),
    }
}

#[async_trait]
impl SvcBackend for BrokerSvc {
    async fn call(&self, topic: &str, mut body: Value, image: Option<SvcImage>, caller: &str) -> Result<Value, ProxyError> {
        tracing::debug!(target: "infer", %topic, %caller, image = image.is_some(), "svc call");
        if let Some(img) = &image {
            body["image_blob"] = self.put_image(img)?;
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().unwrap_or_else(|p| p.into_inner()).insert(id, tx);
        let forget = || {
            self.waiters.lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
        };
        if let Err(e) = self.broker.call_service(&self.session, id, topic, "infer.v1", body, Some(CALL_TIMEOUT.as_millis() as u64)) {
            forget();
            return Err(map_err(e.code, &e.message));
        }
        let ev = match tokio::time::timeout(CALL_TIMEOUT + Duration::from_secs(5), rx).await {
            Ok(Ok(ev)) => ev,
            Ok(Err(_)) => return Err(ProxyError::Upstream("the service session closed".into())),
            Err(_) => {
                forget();
                return Err(ProxyError::Timeout("the service did not answer".into()));
            }
        };
        match ev {
            Event::Ok { result, .. } => Ok(result.get("body").cloned().unwrap_or(Value::Null)),
            Event::Error { code, message, .. } => Err(map_err(code, &message)),
            _ => Err(ProxyError::Upstream("unexpected broker event".into())),
        }
    }
}

impl Drop for BrokerSvc {
    fn drop(&mut self) {
        self.broker.revoke(INSTANCE);
    }
}

impl crate::infer_wire::InferState {
    /// One pass for the roles served by a cogbus service: open the proxy's
    /// session once the broker is up, then serve each role only while its
    /// topic has a provider (so an advertisement is withdrawn when the
    /// provider goes away).
    pub(crate) fn sync_svc(&self) {
        let roles: Vec<&crate::infer_wire::RoleState> = self.roles.iter().filter(|r| r.cfg.svc.is_some()).collect();
        if roles.is_empty() {
            return;
        }
        let none = |why: &str| {
            for r in &roles {
                self.table.deregister_local(&r.cfg.role);
                r.run.lock().unwrap_or_else(|e| e.into_inner()).reason = Some(why.to_string());
            }
        };
        if self.svc.get().is_none() {
            let Some(rt) = crate::cogbus_serve::runtime() else {
                return none("waiting: the cog data plane (cogbus) is not up");
            };
            let project = roles.iter().find_map(|r| r.cfg.svc.as_ref().and_then(|s| s.project.clone()));
            match BrokerSvc::open(rt.broker.clone(), project) {
                Ok(b) => {
                    self.table.set_svc_backend(b.clone());
                    let _ = self.svc.set(b);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "inference svc roles: cannot open the proxy's bus session");
                    return none(&format!("cannot open the bus session: {e}"));
                }
            }
        }
        let Some(rt) = crate::cogbus_serve::runtime() else { return };
        for r in roles {
            let Some(s) = &r.cfg.svc else { continue };
            let mut run = r.run.lock().unwrap_or_else(|e| e.into_inner());
            if rt.broker.serves(&s.topic) {
                run.reason = None;
                drop(run);
                let _ = self.table.register_local_svc(&r.cfg.role, SvcRoute { topic: s.topic.clone(), model: s.model.clone() });
            } else {
                run.reason = Some(format!("waiting: nothing serves {}", s.topic));
                drop(run);
                self.table.deregister_local(&r.cfg.role);
            }
        }
    }
}
