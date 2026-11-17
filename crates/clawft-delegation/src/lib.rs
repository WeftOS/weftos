//! `clawft-delegation` — cognitive work delegation to peer world-model
//! services (WEFT-525).
//!
//! Unblocks peer-to-peer topology for the world-model service (WEFT-524):
//! a local node can forward LatticeApi-shaped work to a remote peer under
//! a signed [`WorkDelegationCert`] (Ed25519 grant / TTL / revoke — same
//! lifecycle ideas as `exo_resource_tree::DelegationCert`, scoped to
//! cognitive methods rather than resource-tree ACLs).
//!
//! # Surface
//!
//! - [`WorkDelegationCert`] — grant / revoke / validate
//! - [`DelegationServer`] / [`LocalDelegationServer`] — handle requests
//! - [`DelegationClient`] / [`InMemoryClient`] — invoke remote methods
//! - Optional [`tcp`] module (`feature = "tcp"`) for blocking TCP round-trips
//!
//! Transport is intentionally thin: default tests use an in-process channel
//! so the crate stays free of kernel / mesh deps.

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use weftos_worldmodel::{
    zero_latent, Action, LatticeApi, LatticeMethod, NodeId, ObservationFrame, StubLattice,
    SubscriptionId, WorldModelError, LATTICE_METHODS, LATENT_DIM, LATENT_DIM_U16,
};

/// Crate version for diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Domain-separation tag prefixed to every signed certificate payload.
pub const CERT_DOMAIN_V1: &[u8] = b"clawft-delegation-cert-v1\0";

/// Service name used when delegating LatticeApi work.
pub const DELEGATION_SERVICE: &str = "worldmodel.lattice";

// ── Errors ─────────────────────────────────────────────────────────────────

/// Delegation-layer errors.
#[derive(Debug, Error, PartialEq)]
pub enum DelegationError {
    /// Certificate signature / encoding issue.
    #[error("bad certificate: {0}")]
    BadCert(String),
    /// Certificate revoked.
    #[error("certificate revoked")]
    Revoked,
    /// Certificate past expiry.
    #[error("certificate expired")]
    Expired,
    /// Method not permitted by the certificate.
    #[error("method not permitted: {0}")]
    MethodDenied(String),
    /// Unknown / unsupported method.
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    /// World-model operation failed.
    #[error("world model error: {0}")]
    WorldModel(String),
    /// CBOR / JSON codec failure.
    #[error("codec error: {0}")]
    Codec(String),
    /// Transport failure (TCP / channel).
    #[error("transport error: {0}")]
    Transport(String),
    /// Missing required certificate on a protected invoke.
    #[error("certificate required")]
    CertRequired,
}

impl From<WorldModelError> for DelegationError {
    fn from(e: WorldModelError) -> Self {
        Self::WorldModel(e.to_string())
    }
}

// ── Certificate (DelegationCert concepts) ──────────────────────────────────

/// Methods a certificate may authorize (subset of LatticeApi).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MethodName(pub String);

impl MethodName {
    /// Construct from a LatticeMethod.
    pub fn from_lattice(m: LatticeMethod) -> Self {
        Self(m.as_str().into())
    }
}

/// Signed grant of cognitive work to a peer (wraps DelegationCert ideas).
///
/// Signable payload (v1, see [`WorkDelegationCert::canonical_message`]): a
/// domain tag followed by length-prefixed fields, so no choice of field
/// contents can shift a boundary and reproduce another cert's bytes.
///
/// A valid signature only proves the holder of `grantor_pubkey` signed the
/// cert. Whether that key is allowed to grant anything is decided by the
/// server's trust anchors (see [`LocalDelegationServer::trust_grantor`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkDelegationCert {
    /// Principal that granted the delegation.
    pub grantor: String,
    /// Principal (or peer node id) receiving the grant.
    pub grantee: String,
    /// Allowed LatticeApi method names (`observe`, `predict`, …). Empty =
    /// all seven methods.
    pub methods: Vec<String>,
    /// Creation time (unix seconds).
    pub created_unix: u64,
    /// Expiry (unix seconds); `None` = no expiry.
    pub expires_unix: Option<u64>,
    /// Revocation bit (local state).
    pub revoked: bool,
    /// Grantor Ed25519 public key (exactly 32 bytes).
    pub grantor_pubkey: Vec<u8>,
    /// Ed25519 signature (exactly 64 bytes) over the canonical payload.
    pub signature: Vec<u8>,
    /// Optional parent cert signature (chain link, 64 bytes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_signature: Option<Vec<u8>>,
}

impl WorkDelegationCert {
    /// Canonical signable message bytes (format v1).
    ///
    /// Layout: [`CERT_DOMAIN_V1`], then `grantor`, `grantee` (each a `u32`
    /// big-endian length plus bytes), the method count (`u32`) and each
    /// method length-prefixed, `created_unix` (`u64` big-endian), an
    /// optional `expires_unix` (`0x00`, or `0x01` plus `u64`), and an
    /// optional parent signature (`0x00`, or `0x01` plus length-prefixed
    /// bytes). Certs signed with the earlier `a|b|c` text format do not
    /// verify under this encoding and must be re-granted.
    pub fn canonical_message(&self) -> Vec<u8> {
        fn put(out: &mut Vec<u8>, bytes: &[u8]) {
            let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(bytes);
        }
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(CERT_DOMAIN_V1);
        put(&mut out, self.grantor.as_bytes());
        put(&mut out, self.grantee.as_bytes());
        out.extend_from_slice(&u32::try_from(self.methods.len()).unwrap_or(u32::MAX).to_be_bytes());
        for m in &self.methods {
            put(&mut out, m.as_bytes());
        }
        out.extend_from_slice(&self.created_unix.to_be_bytes());
        match self.expires_unix {
            Some(t) => {
                out.push(1);
                out.extend_from_slice(&t.to_be_bytes());
            }
            None => out.push(0),
        }
        match self.parent_signature.as_deref() {
            Some(p) => {
                out.push(1);
                put(&mut out, p);
            }
            None => out.push(0),
        }
        out
    }

    /// Grant a new certificate signed by `grantor_key`.
    pub fn grant(
        grantor: impl Into<String>,
        grantee: impl Into<String>,
        methods: Vec<String>,
        grantor_key: &SigningKey,
        created_unix: u64,
        expires_unix: Option<u64>,
        parent: Option<&WorkDelegationCert>,
    ) -> Self {
        let mut cert = Self {
            grantor: grantor.into(),
            grantee: grantee.into(),
            methods,
            created_unix,
            expires_unix,
            revoked: false,
            grantor_pubkey: grantor_key.verifying_key().to_bytes().to_vec(),
            signature: vec![0u8; 64],
            parent_signature: parent.map(|p| p.signature.clone()),
        };
        let msg = cert.canonical_message();
        cert.signature = grantor_key.sign(&msg).to_bytes().to_vec();
        cert
    }

    /// Grant all seven LatticeApi methods.
    pub fn grant_all(
        grantor: impl Into<String>,
        grantee: impl Into<String>,
        grantor_key: &SigningKey,
        created_unix: u64,
        expires_unix: Option<u64>,
    ) -> Self {
        Self::grant(
            grantor,
            grantee,
            Vec::new(), // empty = all
            grantor_key,
            created_unix,
            expires_unix,
            None,
        )
    }

    /// Mark revoked.
    pub fn revoke(&mut self) {
        self.revoked = true;
    }

    /// Verify Ed25519 signature.
    pub fn verify_signature(&self) -> Result<(), DelegationError> {
        if self.grantor_pubkey.len() != 32 {
            return Err(DelegationError::BadCert("pubkey length".into()));
        }
        if self.signature.len() != 64 {
            return Err(DelegationError::BadCert("signature length".into()));
        }
        let mut pk = [0u8; 32];
        pk.copy_from_slice(&self.grantor_pubkey);
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(&self.signature);
        let vk = VerifyingKey::from_bytes(&pk)
            .map_err(|e| DelegationError::BadCert(e.to_string()))?;
        let sig = Signature::from_bytes(&sig_bytes);
        vk.verify(&self.canonical_message(), &sig)
            .map_err(|_| DelegationError::BadCert("verification failed".into()))
    }

    /// Validate signature + revoke + expiry at `now_unix`.
    pub fn validate(&self, now_unix: u64) -> Result<(), DelegationError> {
        if self.revoked {
            return Err(DelegationError::Revoked);
        }
        if let Some(exp) = self.expires_unix
            && now_unix > exp {
                return Err(DelegationError::Expired);
            }
        self.verify_signature()
    }

    /// Whether `method` is permitted (empty methods list = all LatticeApi).
    pub fn permits(&self, method: &str) -> bool {
        if self.methods.is_empty() {
            return LATTICE_METHODS.iter().any(|m| m.as_str() == method);
        }
        self.methods.iter().any(|m| m == method)
    }
}

// ── Request / response ─────────────────────────────────────────────────────

/// Delegated LatticeApi call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationRequest {
    /// Target service name (usually [`DELEGATION_SERVICE`]).
    pub service: String,
    /// LatticeApi method snake_case name.
    pub method: String,
    /// JSON params (ServiceApi-compatible shape).
    pub params: serde_json::Value,
    /// Optional authorization certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert: Option<WorkDelegationCert>,
}

/// Response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegationResponse {
    /// Ok payload or error object.
    pub result: serde_json::Value,
    /// Whether the call succeeded.
    pub ok: bool,
}

// ── Traits ─────────────────────────────────────────────────────────────────

/// Server side: handle a delegated request.
pub trait DelegationServer {
    /// Handle one request; return a response envelope.
    fn handle(&mut self, req: DelegationRequest) -> Result<DelegationResponse, DelegationError>;
}

/// Client side: invoke a method on a remote/local server.
pub trait DelegationClient {
    /// Invoke `method` with JSON `params`, optional cert.
    fn invoke(
        &mut self,
        method: &str,
        params: serde_json::Value,
        cert: Option<WorkDelegationCert>,
    ) -> Result<serde_json::Value, DelegationError>;
}

// ── Local server over LatticeApi ───────────────────────────────────────────

/// In-process server wrapping a [`LatticeApi`] implementation.
pub struct LocalDelegationServer<L: LatticeApi> {
    /// Service name this server answers for.
    pub service: String,
    lattice: L,
    /// When true, requests without a valid cert are rejected.
    pub require_cert: bool,
    /// Clock for cert expiry (injectable for tests).
    pub now_unix: u64,
    /// Ed25519 public keys whose certs this server accepts. Empty means no
    /// cert is accepted (fail closed).
    trusted_grantors: Vec<[u8; 32]>,
}

impl LocalDelegationServer<StubLattice> {
    /// Server over the default stub lattice.
    pub fn stub() -> Self {
        Self {
            service: DELEGATION_SERVICE.into(),
            lattice: StubLattice::default(),
            require_cert: false,
            now_unix: 0,
            trusted_grantors: Vec::new(),
        }
    }
}

impl<L: LatticeApi> LocalDelegationServer<L> {
    /// Wrap an arbitrary lattice.
    pub fn new(lattice: L) -> Self {
        Self {
            service: DELEGATION_SERVICE.into(),
            lattice,
            require_cert: false,
            now_unix: 0,
            trusted_grantors: Vec::new(),
        }
    }

    /// Accept certs signed by `grantor_pubkey`. Without any trusted grantor
    /// every presented cert is rejected, even one with a valid signature.
    pub fn trust_grantor(&mut self, grantor_pubkey: [u8; 32]) {
        if !self.trusted_grantors.contains(&grantor_pubkey) {
            self.trusted_grantors.push(grantor_pubkey);
        }
    }

    /// Builder form of [`Self::trust_grantor`].
    #[must_use]
    pub fn with_trusted_grantor(mut self, grantor_pubkey: [u8; 32]) -> Self {
        self.trust_grantor(grantor_pubkey);
        self
    }

    fn authorize(&self, req: &DelegationRequest) -> Result<(), DelegationError> {
        match &req.cert {
            Some(cert) => {
                cert.validate(self.now_unix)?;
                if !self
                    .trusted_grantors
                    .iter()
                    .any(|k| k.as_slice() == cert.grantor_pubkey.as_slice())
                {
                    return Err(DelegationError::BadCert("grantor not trusted".into()));
                }
                if !cert.permits(&req.method) {
                    return Err(DelegationError::MethodDenied(req.method.clone()));
                }
                Ok(())
            }
            None if self.require_cert => Err(DelegationError::CertRequired),
            None => Ok(()),
        }
    }

    fn dispatch(
        &mut self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, DelegationError> {
        match method {
            "observe" => {
                let bytes = params_bytes(params, "bytes")?;
                let ts = params_u64(params, "timestamp_ms").unwrap_or(0);
                let frame = ObservationFrame {
                    bytes: &bytes,
                    latent_dim: LATENT_DIM_U16,
                    timestamp_ms: ts,
                };
                let z = self.lattice.observe(frame)?;
                Ok(json_latent(&z))
            }
            "observe_node" => {
                let node = params_u64(params, "node").unwrap_or(0);
                let bytes = params_bytes(params, "bytes")?;
                let ts = params_u64(params, "timestamp_ms").unwrap_or(0);
                let frame = ObservationFrame {
                    bytes: &bytes,
                    latent_dim: LATENT_DIM_U16,
                    timestamp_ms: ts,
                };
                let z = self.lattice.observe_node(NodeId(node), frame)?;
                Ok(json_latent(&z))
            }
            "predict" => {
                let z = params_latent(params, "z_t")?;
                let action = params_action(params)?;
                let z_hat = self.lattice.predict(&z, &action)?;
                Ok(json_latent(&z_hat))
            }
            "plan" => {
                let z = params_latent(params, "z_t")?;
                let horizon = params_u64(params, "horizon").unwrap_or(4) as usize;
                let plan = self.lattice.plan(&z, horizon)?;
                Ok(serde_json::json!({
                    "horizon": plan.steps.len(),
                    "kind": format!("{:?}", plan.kind),
                }))
            }
            "recall" => {
                let z = params_latent(params, "query")?;
                let k = params_u64(params, "k").unwrap_or(5) as usize;
                let hits = self.lattice.recall(&z, k)?;
                Ok(serde_json::json!({
                    "hits": hits.iter().map(|h| serde_json::json!({
                        "node": h.node.0,
                        "score": h.score,
                    })).collect::<Vec<_>>(),
                }))
            }
            "subscribe_surprise" => {
                let handle = params_u64(params, "handle").unwrap_or(0);
                self.lattice
                    .subscribe_surprise(SubscriptionId(handle))?;
                Ok(serde_json::json!({"subscribed": "surprise", "handle": handle}))
            }
            "subscribe_drift" => {
                let handle = params_u64(params, "handle").unwrap_or(0);
                self.lattice.subscribe_drift(SubscriptionId(handle))?;
                Ok(serde_json::json!({"subscribed": "drift", "handle": handle}))
            }
            other => Err(DelegationError::UnknownMethod(other.into())),
        }
    }
}

impl<L: LatticeApi> DelegationServer for LocalDelegationServer<L> {
    fn handle(&mut self, req: DelegationRequest) -> Result<DelegationResponse, DelegationError> {
        if req.service != self.service {
            return Err(DelegationError::Transport(format!(
                "service mismatch: {}",
                req.service
            )));
        }
        self.authorize(&req)?;
        match self.dispatch(&req.method, &req.params) {
            Ok(result) => Ok(DelegationResponse { result, ok: true }),
            Err(e) => Ok(DelegationResponse {
                result: serde_json::json!({"error": e.to_string()}),
                ok: false,
            }),
        }
    }
}

// ── In-memory client ───────────────────────────────────────────────────────

/// Client that calls a shared [`LocalDelegationServer`] (round-trip tests).
pub struct InMemoryClient<L: LatticeApi> {
    server: Arc<Mutex<LocalDelegationServer<L>>>,
}

impl<L: LatticeApi> InMemoryClient<L> {
    /// Bind to a shared server.
    pub fn new(server: Arc<Mutex<LocalDelegationServer<L>>>) -> Self {
        Self { server }
    }
}

impl<L: LatticeApi> DelegationClient for InMemoryClient<L> {
    fn invoke(
        &mut self,
        method: &str,
        params: serde_json::Value,
        cert: Option<WorkDelegationCert>,
    ) -> Result<serde_json::Value, DelegationError> {
        let req = DelegationRequest {
            service: DELEGATION_SERVICE.into(),
            method: method.into(),
            params,
            cert,
        };
        let mut guard = self
            .server
            .lock()
            .map_err(|e| DelegationError::Transport(e.to_string()))?;
        let resp = guard.handle(req)?;
        if resp.ok {
            Ok(resp.result)
        } else {
            Err(DelegationError::WorldModel(
                resp.result
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .into(),
            ))
        }
    }
}

// ── TCP transport (optional) ───────────────────────────────────────────────

/// Blocking length-prefixed JSON TCP transport for peer smoke tests.
#[cfg(feature = "tcp")]
pub mod tcp {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream, ToSocketAddrs};

    fn write_msg(stream: &mut TcpStream, value: &impl Serialize) -> Result<(), DelegationError> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| DelegationError::Codec(e.to_string()))?;
        let len = (bytes.len() as u32).to_be_bytes();
        stream
            .write_all(&len)
            .and_then(|_| stream.write_all(&bytes))
            .map_err(|e| DelegationError::Transport(e.to_string()))
    }

    fn read_msg<T: for<'de> Deserialize<'de>>(
        stream: &mut TcpStream,
    ) -> Result<T, DelegationError> {
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .map_err(|e| DelegationError::Transport(e.to_string()))?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 16 * 1024 * 1024 {
            return Err(DelegationError::Transport("message too large".into()));
        }
        let mut buf = vec![0u8; len];
        stream
            .read_exact(&mut buf)
            .map_err(|e| DelegationError::Transport(e.to_string()))?;
        serde_json::from_slice(&buf).map_err(|e| DelegationError::Codec(e.to_string()))
    }

    /// Serve requests on `addr` until one connection handles `max_requests`
    /// (0 = unlimited until peer closes). Blocking.
    pub fn serve_once<L: LatticeApi>(
        addr: impl ToSocketAddrs,
        server: &mut LocalDelegationServer<L>,
        max_requests: usize,
    ) -> Result<(), DelegationError> {
        let listener =
            TcpListener::bind(addr).map_err(|e| DelegationError::Transport(e.to_string()))?;
        let (mut stream, _) = listener
            .accept()
            .map_err(|e| DelegationError::Transport(e.to_string()))?;
        let mut n = 0usize;
        loop {
            if max_requests > 0 && n >= max_requests {
                break;
            }
            let req: DelegationRequest = match read_msg(&mut stream) {
                Ok(r) => r,
                Err(DelegationError::Transport(_)) => break,
                Err(e) => return Err(e),
            };
            let resp = server.handle(req)?;
            write_msg(&mut stream, &resp)?;
            n += 1;
        }
        Ok(())
    }

    /// TCP client.
    pub struct TcpClient {
        stream: TcpStream,
    }

    impl TcpClient {
        /// Connect to a peer.
        pub fn connect(addr: impl ToSocketAddrs) -> Result<Self, DelegationError> {
            let stream =
                TcpStream::connect(addr).map_err(|e| DelegationError::Transport(e.to_string()))?;
            Ok(Self { stream })
        }
    }

    impl DelegationClient for TcpClient {
        fn invoke(
            &mut self,
            method: &str,
            params: serde_json::Value,
            cert: Option<WorkDelegationCert>,
        ) -> Result<serde_json::Value, DelegationError> {
            let req = DelegationRequest {
                service: DELEGATION_SERVICE.into(),
                method: method.into(),
                params,
                cert,
            };
            write_msg(&mut self.stream, &req)?;
            let resp: DelegationResponse = read_msg(&mut self.stream)?;
            if resp.ok {
                Ok(resp.result)
            } else {
                Err(DelegationError::WorldModel(
                    resp.result
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .into(),
                ))
            }
        }
    }
}

// ── JSON helpers ───────────────────────────────────────────────────────────

fn params_bytes(params: &serde_json::Value, key: &str) -> Result<Vec<u8>, DelegationError> {
    match params.get(key) {
        Some(serde_json::Value::String(s)) => Ok(s.as_bytes().to_vec()),
        Some(serde_json::Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                let n = v
                    .as_u64()
                    .ok_or_else(|| DelegationError::Codec(format!("bad byte in {key}")))?;
                out.push(n as u8);
            }
            Ok(out)
        }
        _ => Ok(Vec::new()),
    }
}

fn params_u64(params: &serde_json::Value, key: &str) -> Option<u64> {
    params.get(key).and_then(|v| v.as_u64())
}

fn params_latent(
    params: &serde_json::Value,
    key: &str,
) -> Result<[f32; LATENT_DIM], DelegationError> {
    let arr = params
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| DelegationError::Codec(format!("missing latent array {key}")))?;
    if arr.len() != LATENT_DIM {
        return Err(DelegationError::WorldModel(format!(
            "latent dim mismatch: {}",
            arr.len()
        )));
    }
    let mut z = zero_latent();
    for (i, v) in arr.iter().enumerate() {
        z[i] = v.as_f64().unwrap_or(0.0) as f32;
    }
    Ok(z)
}

fn params_action(params: &serde_json::Value) -> Result<Action, DelegationError> {
    let mut action = Action::null();
    if let Some(arr) = params.get("action").and_then(|v| v.as_array()) {
        for (i, v) in arr.iter().take(32).enumerate() {
            action.code[i] = v.as_f64().unwrap_or(0.0) as f32;
        }
    }
    Ok(action)
}

fn json_latent(z: &[f32; LATENT_DIM]) -> serde_json::Value {
    serde_json::json!({ "latent": z.as_slice(), "latent_dim": LATENT_DIM })
}

/// Registry of peer endpoints (string addresses) for multi-peer hosts.
#[derive(Debug, Default)]
pub struct PeerDirectory {
    peers: HashMap<String, String>,
}

impl PeerDirectory {
    /// Empty directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `peer_id` → `addr`.
    pub fn insert(&mut self, peer_id: impl Into<String>, addr: impl Into<String>) {
        self.peers.insert(peer_id.into(), addr.into());
    }

    /// Look up a peer address.
    pub fn get(&self, peer_id: &str) -> Option<&str> {
        self.peers.get(peer_id).map(|s| s.as_str())
    }

    /// Number of registered peers.
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn cert_grant_validate_revoke() {
        let k = key(1);
        let cert = WorkDelegationCert::grant_all("root", "peer-a", &k, 1000, Some(2000));
        cert.validate(1500).unwrap();
        assert!(cert.permits("observe"));
        assert!(cert.permits("subscribe_drift"));
        assert_eq!(cert.validate(2500), Err(DelegationError::Expired));

        // No-expiry cert (re-grant — mutating expires after sign breaks the sig).
        let mut open = WorkDelegationCert::grant_all("root", "peer-a", &k, 1000, None);
        open.validate(99999).unwrap();
        open.revoke();
        assert_eq!(open.validate(1000), Err(DelegationError::Revoked));
    }

    #[test]
    fn cert_method_subset() {
        let k = key(2);
        let cert = WorkDelegationCert::grant(
            "root",
            "peer",
            vec!["observe".into(), "predict".into()],
            &k,
            0,
            None,
            None,
        );
        assert!(cert.permits("observe"));
        assert!(!cert.permits("plan"));
    }

    #[test]
    fn in_memory_roundtrip_all_seven_methods() {
        let server = Arc::new(Mutex::new(LocalDelegationServer::stub()));
        let mut client = InMemoryClient::new(Arc::clone(&server));

        let z = client
            .invoke(
                "observe",
                serde_json::json!({"bytes": "hello", "timestamp_ms": 1}),
                None,
            )
            .unwrap();
        assert_eq!(z["latent_dim"], LATENT_DIM);

        let z_node = client
            .invoke(
                "observe_node",
                serde_json::json!({"node": 7, "bytes": "n", "timestamp_ms": 2}),
                None,
            )
            .unwrap();
        assert!(z_node["latent"].as_array().unwrap().len() == LATENT_DIM);

        let pred = client
            .invoke(
                "predict",
                serde_json::json!({
                    "z_t": vec![0.0f32; LATENT_DIM],
                    "action": vec![0.0f32; 32],
                }),
                None,
            )
            .unwrap();
        assert_eq!(pred["latent_dim"], LATENT_DIM);

        let plan = client
            .invoke(
                "plan",
                serde_json::json!({"z_t": vec![0.0f32; LATENT_DIM], "horizon": 3}),
                None,
            )
            .unwrap();
        assert_eq!(plan["horizon"], 3);

        let recall = client
            .invoke(
                "recall",
                serde_json::json!({"query": vec![0.0f32; LATENT_DIM], "k": 2}),
                None,
            )
            .unwrap();
        assert!(recall["hits"].as_array().unwrap().is_empty());

        client
            .invoke(
                "subscribe_surprise",
                serde_json::json!({"handle": 1}),
                None,
            )
            .unwrap();
        client
            .invoke("subscribe_drift", serde_json::json!({"handle": 2}), None)
            .unwrap();
    }

    #[test]
    fn cert_required_path() {
        let mut server = LocalDelegationServer::stub();
        server.require_cert = true;
        server.now_unix = 100;
        let k = key(3);
        server.trust_grantor(k.verifying_key().to_bytes());
        let cert = WorkDelegationCert::grant(
            "root",
            "peer",
            vec!["observe".into()],
            &k,
            0,
            Some(500),
            None,
        );

        let shared = Arc::new(Mutex::new(server));
        let mut client = InMemoryClient::new(Arc::clone(&shared));

        let err = client
            .invoke("observe", serde_json::json!({"bytes": "x"}), None)
            .unwrap_err();
        assert_eq!(err, DelegationError::CertRequired);

        // Wrong method denied (authorize fails before dispatch).
        let err = client
            .invoke(
                "plan",
                serde_json::json!({"z_t": vec![0.0f32; LATENT_DIM]}),
                Some(cert.clone()),
            )
            .unwrap_err();
        assert!(matches!(err, DelegationError::MethodDenied(_)));

        client
            .invoke(
                "observe",
                serde_json::json!({"bytes": "ok"}),
                Some(cert),
            )
            .unwrap();
    }

    fn request(method: &str, cert: Option<WorkDelegationCert>) -> DelegationRequest {
        DelegationRequest {
            service: DELEGATION_SERVICE.into(),
            method: method.into(),
            params: serde_json::json!({"bytes": "x"}),
            cert,
        }
    }

    #[test]
    fn self_signed_cert_is_rejected_without_trust_anchor() {
        // Anyone can mint a keypair and sign a cert for themselves; a server
        // that has not been told to trust that key must refuse it.
        let attacker = key(66);
        let cert = WorkDelegationCert::grant_all("mallory", "mallory", &attacker, 0, None);
        let mut server = LocalDelegationServer::stub();
        let err = server.handle(request("observe", Some(cert))).unwrap_err();
        assert!(matches!(err, DelegationError::BadCert(_)), "got {err:?}");
    }

    #[test]
    fn cert_from_trusted_grantor_is_accepted_untrusted_peer_is_not() {
        let root = key(8);
        let other = key(9);
        let mut server =
            LocalDelegationServer::stub().with_trusted_grantor(root.verifying_key().to_bytes());
        let good = WorkDelegationCert::grant_all("root", "peer", &root, 0, None);
        assert!(server.handle(request("observe", Some(good))).unwrap().ok);
        let bad = WorkDelegationCert::grant_all("root", "peer", &other, 0, None);
        assert!(matches!(
            server.handle(request("observe", Some(bad))),
            Err(DelegationError::BadCert(_))
        ));
    }

    #[test]
    fn trusted_grantor_server_refuses_request_without_cert() {
        let root = key(10);
        let mut server =
            LocalDelegationServer::stub().with_trusted_grantor(root.verifying_key().to_bytes());
        let err = server.handle(request("observe", None)).unwrap_err();
        assert!(matches!(err, DelegationError::CertRequired), "got {err:?}");
    }

    #[test]
    fn canonical_message_golden_v1() {
        let k = key(1);
        let c = WorkDelegationCert::grant("g", "p", vec!["observe".into()], &k, 5, Some(9), None);
        let mut want = b"clawft-delegation-cert-v1\0".to_vec();
        want.extend_from_slice(&[0, 0, 0, 1, b'g', 0, 0, 0, 1, b'p', 0, 0, 0, 1]);
        want.extend_from_slice(&[0, 0, 0, 7]);
        want.extend_from_slice(b"observe");
        want.extend_from_slice(&5u64.to_be_bytes());
        want.push(1);
        want.extend_from_slice(&9u64.to_be_bytes());
        want.push(0);
        assert_eq!(c.canonical_message(), want);
    }

    #[test]
    fn payload_fields_cannot_be_resplit() {
        let k = key(7);
        // Field boundary shifted across grantor/grantee.
        let a = WorkDelegationCert::grant("a|b", "c", vec![], &k, 1, None, None);
        let b = WorkDelegationCert::grant("a", "b|c", vec![], &k, 1, None, None);
        assert_ne!(a.canonical_message(), b.canonical_message());
        assert_ne!(a.signature, b.signature);
        // One method containing the separator vs two methods.
        let one = WorkDelegationCert::grant("g", "p", vec!["observe,plan".into()], &k, 1, None, None);
        let two = WorkDelegationCert::grant(
            "g",
            "p",
            vec!["observe".into(), "plan".into()],
            &k,
            1,
            None,
            None,
        );
        assert_ne!(one.canonical_message(), two.canonical_message());
        assert_ne!(one.signature, two.signature);
        // A signature minted for the split-apart cert must not verify on the other.
        let mut forged = b.clone();
        forged.signature = a.signature.clone();
        assert!(forged.verify_signature().is_err());
    }

    #[test]
    fn peer_directory() {
        let mut d = PeerDirectory::new();
        assert!(d.is_empty());
        d.insert("peer-a", "127.0.0.1:9001");
        assert_eq!(d.get("peer-a"), Some("127.0.0.1:9001"));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn unknown_method_errors() {
        let mut server = LocalDelegationServer::stub();
        let resp = server
            .handle(DelegationRequest {
                service: DELEGATION_SERVICE.into(),
                method: "nope".into(),
                params: serde_json::json!({}),
                cert: None,
            })
            .unwrap();
        assert!(!resp.ok);
    }
}
