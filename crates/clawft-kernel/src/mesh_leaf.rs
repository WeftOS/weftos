//! Parent-side ADR-103 leaf admission and replay floor.
//!
//! The machine mesh service owns this state beside its journal. In a
//! collapsed kernel the same component lives under its isolated runtime
//! root. The enrolled public certificate is the explicit admission grant;
//! removing it revokes future frames. No leaf seed is stored here.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use clawft_types::project::cert::{key_id, ProjectCert};
use serde::{Deserialize, Serialize};
use weftos_leaf_types::link::{LeafCertificate, ParentAdvertisement, PublishAck, SignedPublish};

pub const MAX_LEAF_FRAME: usize = 32 * 1024;
pub const MAX_LEAF_PAYLOAD: usize = 16 * 1024;
pub const DISCOVERY_MAGIC: &[u8; 4] = b"WLD1";
const MAX_ENROLLMENT_FILE: usize = 4096;
const MAX_FLOOR_FILE: usize = 512;

fn private_dir(path: &Path) -> Result<(), LeafError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_dir() => return Err(LeafError::Artifact(format!("{} is not a directory", path.display()))),
        Ok(_) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)] {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)?;
        },
        Err(e) => return Err(e.into()),
    }
    let meta = fs::symlink_metadata(path)?;
    #[cfg(unix)] {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o077 != 0 {
            return Err(LeafError::Artifact(format!("{} must be owned by the service and mode 0700", path.display())));
        }
    }
    Ok(())
}

fn read_bounded(path: &Path, max: usize) -> Result<Option<Vec<u8>>, LeafError> {
    let mut opts = OpenOptions::new(); opts.read(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let meta = f.metadata()?;
    if !meta.is_file() || meta.len() > max as u64 {
        return Err(LeafError::Artifact(format!("{} is not a bounded regular file", path.display())));
    }
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } {
            return Err(LeafError::Artifact(format!("{} is not owned by the service", path.display())));
        }
    }
    let mut bytes = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max { return Err(LeafError::Artifact(format!("{} exceeds {} bytes", path.display(), max))); }
    Ok(Some(bytes))
}

#[derive(Debug, thiserror::Error)]
pub enum LeafError {
    #[error("leaf io: {0}")]
    Io(#[from] std::io::Error),
    #[error("leaf artifact: {0}")]
    Artifact(String),
    #[error("leaf certificate is not enrolled or has been revoked")]
    NotEnrolled,
    #[error("leaf certificate/signature: {0:?}")]
    Signature(weftos_leaf_types::link::LinkError),
    #[error("leaf sequence gap or regression: expected {expected}, got {got}")]
    Sequence { expected: u64, got: u64 },
    #[error("leaf sequence reused with different content")]
    Equivocation,
    #[error("leaf payload exceeds {MAX_LEAF_PAYLOAD} bytes")]
    TooLarge,
    #[error("leaf parent does not own the certified tenant scope: {0}")]
    WrongScope(String),
}

#[derive(Deserialize)]
struct Enrollment {
    node_id: String,
    cert: LeafCertificate,
    #[serde(default)]
    project_cert: Option<ProjectCert>,
}

fn hex_key(s: &str) -> Option<[u8; 32]> {
    let raw = hex::decode(s).ok()?;
    raw.try_into().ok()
}

/// Prove the leaf issuer owns its requested tenant. A user issuer must be
/// that user's key. A project issuer needs the user's signed project cert,
/// with both key IDs recomputed by `ProjectCert::verify`.
pub fn verify_scope_chain(cert: &LeafCertificate, project_cert: Option<&ProjectCert>, now: u64) -> Result<[u8; 32], LeafError> {
    let (user_id, project_id) = weftos_leaf_types::link::parse_parent_scope(&cert.parent_scope)
        .map_err(|e| LeafError::WrongScope(format!("scope syntax: {e:?}")))?;
    match project_id {
        None => {
            if project_cert.is_some() || key_id(&cert.parent_pubkey) != user_id {
                return Err(LeafError::WrongScope("user issuer key ID does not match user scope".into()));
            }
            Ok(cert.parent_pubkey)
        }
        Some(project_id) => {
            let pc = project_cert.ok_or_else(|| LeafError::WrongScope("project certificate missing".into()))?;
            let user_key = hex_key(&pc.user_pubkey).ok_or_else(|| LeafError::WrongScope("project user key malformed".into()))?;
            let at = i64::try_from(now).ok().and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
                .ok_or_else(|| LeafError::WrongScope("invalid verification time".into()))?;
            pc.verify(&user_key, at).map_err(|e| LeafError::WrongScope(format!("project certificate: {e}")))?;
            if pc.project_id != project_id || pc.project_key_id != key_id(&cert.parent_pubkey)
                || pc.user_key_id != user_id || hex_key(&pc.project_pubkey) != Some(cert.parent_pubkey) {
                return Err(LeafError::WrongScope("project certificate does not bind issuer and tenant".into()));
            }
            Ok(user_key)
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Floor {
    seq: u64,
    digest: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prepare { New, Duplicate }

type ScopeAuthority = std::sync::Arc<dyn Fn(&str, &[u8; 32], Option<&ProjectCert>) -> bool + Send + Sync>;

pub struct LeafIngress {
    registry: PathBuf,
    floors: PathBuf,
    machine_key: SigningKey,
    scope_authority: Option<ScopeAuthority>,
    /// Serialize a leaf's check, delivery and floor commit. Held across the
    /// runtime call; callers use Tokio's async mutex guard.
    pub lock: tokio::sync::Mutex<()>,
}

impl LeafIngress {
    /// `root/registry/<id>.json` holds enrollment artifacts; `root/floors`
    /// is service-owned and must never be writable by a leaf process.
    pub fn open(root: &Path, machine_key: SigningKey) -> Result<Self, LeafError> {
        let registry = root.join("registry");
        let floors = root.join("floors");
        private_dir(root)?;
        private_dir(&registry)?;
        private_dir(&floors)?;
        Ok(Self { registry, floors, machine_key, scope_authority: None, lock: tokio::sync::Mutex::new(()) })
    }

    /// Machine-service live binding check. The callback must prove the
    /// registered tenant currently holds the user key named by the chain.
    pub fn with_scope_authority(mut self, authority: ScopeAuthority) -> Self {
        self.scope_authority = Some(authority);
        self
    }

    fn authorized(&self, enrolled: &Enrollment, cert: &LeafCertificate, now: u64) -> bool {
        if enrolled.node_id != cert.leaf_id() || enrolled.cert != *cert
            || cert.mesh_pubkey != self.machine_key.verifying_key().to_bytes()
            || cert.verify(&cert.parent_pubkey, now).is_err() { return false; }
        let Ok(user_key) = verify_scope_chain(cert, enrolled.project_cert.as_ref(), now) else { return false };
        let (user_id, _) = weftos_leaf_types::link::parse_parent_scope(&cert.parent_scope).expect("verified scope");
        self.scope_authority.as_ref().is_none_or(|authority| authority(user_id, &user_key, enrolled.project_cert.as_ref()))
    }

    fn floor_path(&self, id: &str) -> PathBuf { self.floors.join(format!("{id}.json")) }

    fn read_floor(&self, id: &str) -> Result<Option<Floor>, LeafError> {
        read_bounded(&self.floor_path(id), MAX_FLOOR_FILE)?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|e| LeafError::Artifact(format!("invalid replay floor: {e}"))))
            .transpose()
    }

    /// Recheck an idle subscribed connection. Enrollment removal, exact
    /// certificate replacement and expiry all terminate its push route.
    pub fn still_enrolled(&self, cert: &LeafCertificate, now: u64) -> bool {
        let id = cert.leaf_id();
        let Ok(Some(bytes)) = read_bounded(&self.registry.join(format!("{id}.json")), MAX_ENROLLMENT_FILE) else { return false };
        let Ok(enrolled) = serde_json::from_slice::<Enrollment>(&bytes) else { return false };
        self.authorized(&enrolled, cert, now)
    }

    /// Validate against the installed certificate and durable replay floor.
    /// `Duplicate` is safe to ACK but must never be delivered again.
    pub fn prepare(&self, frame: &SignedPublish, now: u64) -> Result<Prepare, LeafError> {
        if frame.payload.len() > MAX_LEAF_PAYLOAD { return Err(LeafError::TooLarge); }
        let id = frame.cert.leaf_id();
        let bytes = read_bounded(&self.registry.join(format!("{id}.json")), MAX_ENROLLMENT_FILE)?.ok_or(LeafError::NotEnrolled)?;
        let enrolled: Enrollment = serde_json::from_slice(&bytes).map_err(|e| LeafError::Artifact(format!("invalid enrollment: {e}")))?;
        if !self.authorized(&enrolled, &frame.cert, now) {
            return Err(LeafError::NotEnrolled);
        }
        frame.verify(&enrolled.cert.parent_pubkey, now).map_err(LeafError::Signature)?;
        match self.read_floor(&id)? {
            None if frame.seq == 1 => Ok(Prepare::New),
            None => Err(LeafError::Sequence { expected: 1, got: frame.seq }),
            Some(floor) if frame.seq == floor.seq => {
                if frame.digest() == floor.digest { Ok(Prepare::Duplicate) } else { Err(LeafError::Equivocation) }
            }
            Some(floor) if frame.seq == floor.seq.saturating_add(1) => Ok(Prepare::New),
            Some(floor) => Err(LeafError::Sequence { expected: floor.seq.saturating_add(1), got: frame.seq }),
        }
    }

    /// Call only after delivery returned success. Sync file and directory
    /// before sending an ACK. A crash between delivery and commit can replay
    /// once; downstream operations must be idempotent by `(leaf_id, seq)`.
    pub fn commit(&self, frame: &SignedPublish) -> Result<(), LeafError> {
        let id = frame.cert.leaf_id();
        let path = self.floor_path(&id);
        let tmp = self.floors.join(format!(".{id}.{:016x}.tmp", rand::random::<u64>()));
        let bytes = serde_json::to_vec(&Floor { seq: frame.seq, digest: frame.digest() })
            .map_err(|e| LeafError::Artifact(e.to_string()))?;
        let mut opts = OpenOptions::new(); opts.write(true).create_new(true);
        #[cfg(unix)] {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        let result = (|| -> Result<(), LeafError> {
            f.write_all(&bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
            File::open(&self.floors)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() { let _ = fs::remove_file(&tmp); }
        result
    }

    pub fn ack(&self, frame: &SignedPublish) -> PublishAck { PublishAck::sign(&self.machine_key, frame) }

    fn advertisement(&self, id: &str, endpoint: String, now: u64, nonce: [u8; 16]) -> Option<ParentAdvertisement> {
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) { return None; }
        let bytes = read_bounded(&self.registry.join(format!("{id}.json")), MAX_ENROLLMENT_FILE).ok()??;
        let enrolled: Enrollment = serde_json::from_slice(&bytes).ok()?;
        if !self.authorized(&enrolled, &enrolled.cert, now) { return None; }
        Some(ParentAdvertisement::sign(&self.machine_key, enrolled.cert.parent_scope, endpoint, now.saturating_add(30), nonce))
    }

    /// UDP discovery on `bind` (the leaf bind address; main port + 1 unless
    /// configured), advertising `leaf_port`. Requests
    /// carry `WLD1 | leaf id (32 ASCII hex) | nonce (16)`. The response is a
    /// machine-signed CBOR advertisement bound to that nonce and enrollment.
    pub async fn serve_discovery(&self, bind: std::net::SocketAddr, leaf_port: u16) -> Result<(), LeafError> {
        self.serve_discovery_with(bind, move |peer| {
            let ip = if bind.ip().is_unspecified() { routed_local_ip(bind.ip(), peer)? } else { bind.ip() };
            Some(std::net::SocketAddr::new(ip, leaf_port))
        }).await
    }

    /// Discovery on a wildcard socket (`kernel.mesh.leaf_discovery_any`).
    /// `choose` maps the probe's source to the TCP endpoint to advertise, or
    /// `None` to stay silent (no listener bound yet).
    pub async fn serve_discovery_with(
        &self,
        bind: std::net::SocketAddr,
        choose: impl Fn(std::net::SocketAddr) -> Option<std::net::SocketAddr>,
    ) -> Result<(), LeafError> {
        let sock = tokio::net::UdpSocket::bind(bind).await?;
        let mut request = [0u8; 64];
        loop {
            let (n, peer) = sock.recv_from(&mut request).await?;
            if n != 52 || &request[..4] != DISCOVERY_MAGIC { continue; }
            let Ok(id) = std::str::from_utf8(&request[4..36]) else { continue };
            let mut nonce = [0u8; 16]; nonce.copy_from_slice(&request[36..52]);
            let Some(endpoint) = choose(peer) else { continue };
            let endpoint = endpoint.to_string();
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            if let Some(ad) = self.advertisement(id, endpoint, now, nonce)
                && let Ok(bytes) = weftos_leaf_types::encode(&ad)
                && bytes.len() <= 1024 {
                    let _ = sock.send_to(&bytes, peer).await;
                }
        }
    }
}

/// The local IP the OS would use to reach `peer`, from a connected UDP socket.
fn routed_local_ip(family: std::net::IpAddr, peer: std::net::SocketAddr) -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind(std::net::SocketAddr::new(family, 0)).ok()?;
    probe.connect(peer).ok()?;
    probe.local_addr().ok().map(|a| a.ip())
}

/// Pick which bound leaf TCP endpoint a probe from `peer` is told about:
/// the one on the probe's subnet (IPv4 /24, IPv6 /64), else the one the OS
/// routes to the peer, else the first. `None` only when nothing is bound.
pub fn pick_leaf_endpoint(
    candidates: &[std::net::SocketAddr],
    peer: std::net::SocketAddr,
    routed: Option<std::net::IpAddr>,
) -> Option<std::net::SocketAddr> {
    use std::net::IpAddr;
    let same_subnet = |a: IpAddr, b: IpAddr| match (a, b) {
        (IpAddr::V4(a), IpAddr::V4(b)) => a.octets()[..3] == b.octets()[..3],
        (IpAddr::V6(a), IpAddr::V6(b)) => a.octets()[..8] == b.octets()[..8],
        _ => false,
    };
    candidates.iter().find(|c| same_subnet(c.ip(), peer.ip()))
        .or_else(|| routed.and_then(|r| candidates.iter().find(|c| c.ip() == r)))
        .or_else(|| candidates.first())
        .copied()
}

/// Wildcard discovery for one discovery port: advertises whichever bound
/// leaf listener (from `bounds`) fits the probe's source.
pub async fn serve_wildcard_discovery(
    ingress: &LeafIngress,
    port: u16,
    bounds: impl Fn() -> Vec<std::net::SocketAddr>,
) -> Result<(), LeafError> {
    let bind = std::net::SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), port);
    ingress.serve_discovery_with(bind, move |peer| {
        let candidates = bounds();
        let routed = routed_local_ip(std::net::Ipv4Addr::UNSPECIFIED.into(), peer);
        pick_leaf_endpoint(&candidates, peer, routed)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_types::project::cert::{CertRequest, ProjectCert};
    use ed25519_dalek::SigningKey;
    use weftos_leaf_types::link::node_id;

    #[test]
    fn scope_chain_rejects_another_users_scope_and_requires_project_issuer_proof() {
        let user_a = SigningKey::from_bytes(&[51; 32]);
        let user_b = SigningKey::from_bytes(&[52; 32]);
        let project = SigningKey::from_bytes(&[53; 32]);
        let leaf = SigningKey::from_bytes(&[54; 32]);
        let machine = SigningKey::from_bytes(&[55; 32]);
        let id_a = key_id(&user_a.verifying_key().to_bytes());
        let id_b = key_id(&user_b.verifying_key().to_bytes());
        let now = chrono::Utc::now();
        let secs = now.timestamp() as u64;
        let wrong_user = LeafCertificate::issue(&user_a, format!("user:{id_b}"), machine.verifying_key().to_bytes(),
            leaf.verifying_key().to_bytes(), 1, secs - 1, secs + 60, vec!["input.publish".into()]).unwrap();
        assert!(verify_scope_chain(&wrong_user, None, secs).is_err());

        let project_id = "01HZY7Y9M3MZQ5RSY3MTEP6F3C";
        let chain = ProjectCert::sign(&user_a, &CertRequest {
            project_id: project_id.into(), project_pubkey: project.verifying_key().to_bytes(),
            serial: 1, issued_at: now, expires_at: None,
        });
        let good = LeafCertificate::issue(&project, format!("project:{id_a}:{project_id}"), machine.verifying_key().to_bytes(),
            leaf.verifying_key().to_bytes(), 2, secs - 1, secs + 60, vec!["input.publish".into()]).unwrap();
        assert_eq!(verify_scope_chain(&good, Some(&chain), secs).unwrap(), user_a.verifying_key().to_bytes());
        assert!(verify_scope_chain(&good, None, secs).is_err());
        let forged_scope = LeafCertificate::issue(&project, format!("project:{id_b}:{project_id}"), machine.verifying_key().to_bytes(),
            leaf.verifying_key().to_bytes(), 3, secs - 1, secs + 60, vec!["input.publish".into()]).unwrap();
        assert!(verify_scope_chain(&forged_scope, Some(&chain), secs).is_err());
        let other_project = SigningKey::from_bytes(&[56; 32]);
        let wrong_issuer = LeafCertificate::issue(&other_project, format!("project:{id_a}:{project_id}"), machine.verifying_key().to_bytes(),
            leaf.verifying_key().to_bytes(), 4, secs - 1, secs + 60, vec!["input.publish".into()]).unwrap();
        assert!(verify_scope_chain(&wrong_issuer, Some(&chain), secs).is_err());

        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap(); }
        let expected_user = id_a.clone();
        let expected_key = user_a.verifying_key().to_bytes();
        let ingress = LeafIngress::open(dir.path(), machine).unwrap().with_scope_authority(std::sync::Arc::new(move |id, key, _| {
            id == expected_user && *key == expected_key
        }));
        let leaf_id = good.leaf_id();
        fs::write(ingress.registry.join(format!("{leaf_id}.json")),
            serde_json::json!({"node_id": leaf_id, "cert": good.clone(), "project_cert": chain.clone()}).to_string()).unwrap();
        let frame = SignedPublish::sign(good.clone(), &leaf, 1, format!("mesh.leaf.{leaf_id}.input"), vec![1]).unwrap();
        assert_eq!(ingress.prepare(&frame, secs).unwrap(), Prepare::New);
        let denied = LeafIngress::open(dir.path(), ingress.machine_key.clone()).unwrap()
            .with_scope_authority(std::sync::Arc::new(|_, _, _| false));
        assert!(matches!(denied.prepare(&frame, secs), Err(LeafError::NotEnrolled)));
    }

    #[test]
    fn enrolled_frame_replays_exactly_once_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap(); }
        let parent = SigningKey::from_bytes(&[3;32]);
        let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
        let machine = SigningKey::from_bytes(&[4;32]);
        let leaf = SigningKey::from_bytes(&[5;32]);
        let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, 1, 1000, vec!["input.publish".into()]).unwrap();
        let id = node_id(&cert.leaf_pubkey);
        let frame = SignedPublish::sign(cert.clone(), &leaf, 1, format!("mesh.leaf.{id}.input"), vec![1]).unwrap();
        let ingress = LeafIngress::open(dir.path(), machine.clone()).unwrap();
        fs::write(ingress.registry.join(format!("{id}.json")), serde_json::json!({"node_id":id,"cert":cert}).to_string()).unwrap();
        assert_eq!(ingress.prepare(&frame, 10).unwrap(), Prepare::New);
        ingress.commit(&frame).unwrap();
        let ingress = LeafIngress::open(dir.path(), machine).unwrap();
        assert_eq!(ingress.prepare(&frame, 10).unwrap(), Prepare::Duplicate);
        assert!(ingress.still_enrolled(&frame.cert, 10));
        assert!(!ingress.still_enrolled(&frame.cert, 1000));
        ingress.ack(&frame).verify(&frame.cert.mesh_pubkey, &frame).unwrap();
        let mut altered = frame.clone(); altered.payload.push(2);
        assert!(matches!(ingress.prepare(&altered, 10), Err(LeafError::Signature(_))));
        fs::remove_file(ingress.registry.join(format!("{id}.json"))).unwrap();
        assert!(!ingress.still_enrolled(&frame.cert, 10));
        assert!(matches!(ingress.prepare(&frame, 10), Err(LeafError::NotEnrolled)));
    }

    #[test]
    fn discovery_only_answers_enrolled_leaf_with_signed_endpoint_and_nonce() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap(); }
        let parent = SigningKey::from_bytes(&[13; 32]);
        let user_id = clawft_types::project::cert::key_id(&parent.verifying_key().to_bytes());
        let machine = SigningKey::from_bytes(&[14; 32]);
        let leaf = SigningKey::from_bytes(&[15; 32]);
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let cert = LeafCertificate::issue(&parent, format!("user:{user_id}"), machine.verifying_key().to_bytes(), leaf.verifying_key().to_bytes(), 1, now - 1, now + 60, vec!["input.publish".into()]).unwrap();
        let id = cert.leaf_id();
        let ingress = LeafIngress::open(dir.path(), machine.clone()).unwrap();
        fs::write(ingress.registry.join(format!("{id}.json")), serde_json::json!({"node_id":id,"cert":cert}).to_string()).unwrap();
        let endpoint = "127.0.0.1:9489".to_string();
        let ad = ingress.advertisement(&id, endpoint.clone(), now, [9; 16]).unwrap();
        ad.verify_for(&machine.verifying_key().to_bytes(), &cert.parent_scope, now, &[9; 16]).unwrap();
        assert_eq!(ad.endpoint, endpoint);
        assert!(ad.verify_for(&machine.verifying_key().to_bytes(), &cert.parent_scope, now, &[8; 16]).is_err());
        assert!(ingress.advertisement(&"f".repeat(32), endpoint, now, [9; 16]).is_none());
    }

    #[test]
    fn endpoint_choice_prefers_the_probes_subnet() {
        let a = |s: &str| -> std::net::SocketAddr { s.parse().unwrap() };
        let cands = [a("100.64.0.5:9491"), a("192.168.10.100:9491"), a("10.0.0.2:9491")];
        assert_eq!(pick_leaf_endpoint(&cands, a("192.168.10.149:50000"), None), Some(a("192.168.10.100:9491")));
        assert_eq!(pick_leaf_endpoint(&cands, a("10.0.0.77:1"), None), Some(a("10.0.0.2:9491")));
        // No subnet match: the routed interface wins, then the first entry.
        assert_eq!(pick_leaf_endpoint(&cands, a("172.16.0.9:1"), Some("10.0.0.2".parse().unwrap())), Some(a("10.0.0.2:9491")));
        assert_eq!(pick_leaf_endpoint(&cands, a("172.16.0.9:1"), None), Some(a("100.64.0.5:9491")));
        assert_eq!(pick_leaf_endpoint(&[], a("172.16.0.9:1"), None), None);
    }
}
