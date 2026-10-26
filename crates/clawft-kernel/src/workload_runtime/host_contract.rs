//! Host contract common to every cog runtime (ADR-100 section 4):
//! `COG_CSI_BIND`, `COG_SENSOR_URL`, a per-instance `COGNITUM_COG_TOKEN`
//! and `COGNITUM_COG_DATA_DIR`; with the node's cogbus up, also
//! `WEFTOS_COGBUS` and `WEFTOS_COGBUS_TOKEN` (ADR-122 section 4).
//!
//! `WEFTOS_COGBUS_TOKEN` carries the same per-instance token value as
//! `COGNITUM_COG_TOKEN`: one secret names the instance to both the ingest
//! bridge and the broker, each keeps only its hash, and both are revoked at
//! the same stop and unload. It is a separate variable so a cog never needs
//! to know that, and so a node whose ingest bridge is down (no ingest token)
//! still hands its cogs a broker token.
//!
//! The token is a secret: it is handed to the instance's environment only,
//! and anything written to the chain carries [`HostContract::token_hash`].

use std::net::SocketAddr;

use clawft_types::secret::SecretString;
use rand::RngCore;

use super::types::RuntimeError;

/// Env var: UDP bind address for the ESP32 CSI feed.
pub const ENV_CSI_BIND: &str = "COG_CSI_BIND";
/// Env var: HTTP sensor fallback (`host:port`).
pub const ENV_SENSOR_URL: &str = "COG_SENSOR_URL";
/// Env var: per-instance token accepted by that instance's ingest bridge.
pub const ENV_TOKEN: &str = "COGNITUM_COG_TOKEN";
/// Env var: full URL of this instance's ingest bridge (for cogs that honour
/// it; released cogs post to the fixed `127.0.0.1:80`).
pub const ENV_INGEST_URL: &str = "COGNITUM_INGEST_URL";
/// Env var: writable data directory.
pub const ENV_DATA_DIR: &str = "COGNITUM_COG_DATA_DIR";
/// Env var: path of the node's cogbus socket (ADR-122).
pub const ENV_COGBUS: &str = "WEFTOS_COGBUS";
/// Env var: the instance's cogbus token (the instance token, see above).
pub const ENV_COGBUS_TOKEN: &str = "WEFTOS_COGBUS_TOKEN";
/// Env var: the devices this instance owns (ADR-122 section 4b), `:`-separated
/// absolute paths. Only the device ledger's owner gets it.
pub const ENV_DEVICES: &str = "WEFTOS_DEVICES";
/// Default CSI feed port (ADR-069).
pub const DEFAULT_CSI_PORT: u16 = 5006;

/// The identity a native instance runs as (ADR-122 gap 2): its own uid and
/// gid, plus the supplementary groups it is placed in (the `cogbus` group,
/// which owns the broker socket).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CogIdentity {
    /// Uid to switch to.
    pub uid: u32,
    /// Primary gid.
    pub gid: u32,
    /// Supplementary groups (replaces the daemon's; never empty of the
    /// socket's group when the broker is on).
    pub groups: Vec<u32>,
}

/// Host contract for one instance.
#[derive(Debug, Clone)]
pub struct HostContract {
    /// UDP bind the cog listens on for the sensor feed.
    pub csi_bind: SocketAddr,
    /// HTTP sensor fallback address (`host:port`), if any.
    pub sensor_url: Option<SocketAddr>,
    /// Per-instance token.
    pub token: SecretString,
    /// The node's ingest bridge as the instance's network sees it. A cog
    /// always posts to its own `127.0.0.1:80`; container adapters relay
    /// that port here ([`super::container_relay`]). `None` means the cog's
    /// loopback already reaches the bridge (native, `network=host`).
    pub ingest_upstream: Option<SocketAddr>,
    /// Ingest bridge URL injected as [`ENV_INGEST_URL`], if any.
    pub ingest_url: Option<String>,
    /// False: the instance gets no [`ENV_TOKEN`] (and no ingest URL). Set
    /// when the node's ingest bridge is down, so a cog never carries a
    /// credential for a bridge that is not there.
    pub expose_token: bool,
    /// The node's cogbus socket, injected as [`ENV_COGBUS`] with
    /// [`ENV_COGBUS_TOKEN`]. `None`: no broker for this instance (it is
    /// down, or the route cannot reach a host socket).
    pub cogbus: Option<String>,
    /// Identity the native adapter switches to before exec, when the broker
    /// allocated one for this instance. `None`: the adapter's own `run_as`,
    /// or the daemon's uid.
    pub run_as: Option<CogIdentity>,
    /// Device nodes and sockets this instance owns, injected as
    /// [`ENV_DEVICES`]. Set only through the device ledger's reservation, so
    /// a non-owner is never handed a device path.
    pub devices: Vec<String>,
}

impl HostContract {
    /// Contract binding the feed on `csi_bind`, with a fresh random token.
    pub fn new(csi_bind: SocketAddr) -> Self {
        Self {
            csi_bind,
            sensor_url: None,
            token: Self::fresh_token(),
            ingest_upstream: None,
            ingest_url: None,
            expose_token: true,
            cogbus: None,
            run_as: None,
            devices: Vec::new(),
        }
    }

    /// Default: feed on `0.0.0.0:5006`.
    pub fn default_feed() -> Self {
        Self::new(SocketAddr::from(([0, 0, 0, 0], DEFAULT_CSI_PORT)))
    }

    /// Builder: HTTP sensor fallback.
    pub fn with_sensor(mut self, addr: SocketAddr) -> Self {
        self.sensor_url = Some(addr);
        self
    }

    /// Builder: ingest bridge address as seen from inside the instance.
    pub fn with_ingest_upstream(mut self, addr: SocketAddr) -> Self {
        self.ingest_upstream = Some(addr);
        self
    }

    /// Builder: bridge URL handed to the cog as [`ENV_INGEST_URL`].
    pub fn with_ingest_url(mut self, url: impl Into<String>) -> Self {
        self.ingest_url = Some(url.into());
        self
    }

    /// Builder: the cogbus socket handed to the instance.
    pub fn with_cogbus(mut self, socket: impl Into<String>) -> Self {
        self.cogbus = Some(socket.into());
        self
    }

    /// Builder: the identity the instance runs as.
    pub fn with_run_as(mut self, id: CogIdentity) -> Self {
        self.run_as = Some(id);
        self
    }

    /// Builder: the devices the instance owns.
    pub fn with_devices(mut self, devices: Vec<String>) -> Self {
        self.devices = devices;
        self
    }

    /// Builder: hand the instance no token and no ingest URL.
    pub fn without_ingest(mut self) -> Self {
        self.expose_token = false;
        self.ingest_url = None;
        self
    }

    /// 32 random bytes, lower-case hex.
    pub fn fresh_token() -> SecretString {
        let mut b = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut b);
        SecretString::new(crate::workload_pkg::codec::hex_encode(&b))
    }

    /// Short BLAKE3 fingerprint of the token, safe for chain payloads.
    pub fn token_hash(&self) -> String {
        blake3::hash(self.token.expose().as_bytes()).to_hex()[..16].to_string()
    }

    /// Validate before use.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        if self.csi_bind.port() == 0 {
            return Err(RuntimeError::InvalidConfig("csi_bind needs a port".into()));
        }
        let t = self.token.expose();
        if t.len() < 32 || t.len() > 256 || !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(RuntimeError::InvalidConfig(
                "cog token must be 32..256 printable characters".into(),
            ));
        }
        Ok(())
    }

    /// Environment for the instance. `data_dir` is where the runtime mounts
    /// or creates the instance's writable directory.
    pub fn env(&self, data_dir: &str) -> Vec<(String, String)> {
        let mut env = vec![
            (ENV_CSI_BIND.to_string(), self.csi_bind.to_string()),
            (ENV_DATA_DIR.to_string(), data_dir.to_string()),
        ];
        if self.expose_token {
            env.push((ENV_TOKEN.to_string(), self.token.expose().to_string()));
        }
        if let Some(s) = self.sensor_url {
            env.push((ENV_SENSOR_URL.to_string(), s.to_string()));
        }
        if let Some(u) = &self.ingest_url {
            env.push((ENV_INGEST_URL.to_string(), u.clone()));
        }
        if let Some(p) = &self.cogbus {
            env.push((ENV_COGBUS.to_string(), p.clone()));
            env.push((ENV_COGBUS_TOKEN.to_string(), self.token.expose().to_string()));
        }
        if !self.devices.is_empty() {
            env.push((ENV_DEVICES.to_string(), self.devices.join(":")));
        }
        env
    }

    /// Chain-safe summary (token as hash only).
    pub fn audit(&self) -> serde_json::Value {
        serde_json::json!({
            "csi_bind": self.csi_bind.to_string(),
            "sensor_url": self.sensor_url.map(|s| s.to_string()),
            "token_hash": self.token_hash(),
            "ingest_upstream": self.ingest_upstream.map(|s| s.to_string()),
            "ingest_url": self.ingest_url,
            "cogbus": self.cogbus,
            "devices": self.devices,
        })
    }
}
