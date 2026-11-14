//! `clawft-worldmodel-service` — minimal LeWM WorldModelService binary (WEFT-524).
//!
//! ```text
//! clawft-worldmodel-service --topology single --smoke-frames 3
//! clawft-worldmodel-service --topology hot_standby --role standby
//! clawft-worldmodel-service --topology peer_to_peer --node-id wm-peer-3
//! ```

use clap::Parser;
use clawft_worldmodel_service::{
    DeploymentTopology, ServiceConfig, ServiceRole, WorldModelService,
};

/// LeWM WorldModelService host.
#[derive(Debug, Parser)]
#[command(
    name = "clawft-worldmodel-service",
    about = "WeftOS latent world-model service (single / hot-standby / peer-to-peer)",
    version
)]
struct Cli {
    /// Deployment topology: single | hot_standby | peer_to_peer
    #[arg(long, default_value = "single", env = "WM_TOPOLOGY")]
    topology: String,

    /// Role: primary | standby | peer (defaults from topology)
    #[arg(long, env = "WM_ROLE")]
    role: Option<String>,

    /// Node identifier for multi-node scaffolds
    #[arg(long, default_value = "wm-0", env = "WM_NODE_ID")]
    node_id: String,

    /// After boot, run N frames through a fake sensor pipeline (smoke test)
    #[arg(long, default_value_t = 0, env = "WM_SMOKE_FRAMES")]
    smoke_frames: u64,

    /// Heartbeat lease (ms) a hot standby grants the primary before promoting
    #[arg(long, default_value_t = clawft_worldmodel_service::DEFAULT_LEASE_MS, env = "WM_LEASE_MS")]
    lease_ms: u64,

    /// Use hash encoder/action for non-zero smoke latents
    #[arg(long, default_value_t = false)]
    smoke_hash: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let topology = DeploymentTopology::parse(&cli.topology).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown topology {:?}; expected single | hot_standby | peer_to_peer",
            cli.topology
        )
    })?;

    let mut config = ServiceConfig::for_topology(topology);
    config.node_id = cli.node_id;
    config.smoke_frames = cli.smoke_frames;
    config.lease_ms = cli.lease_ms;
    if let Some(role_s) = cli.role.as_deref() {
        config.role = parse_role(role_s)?;
    }

    tracing::info!(
        topology = topology.as_str(),
        label = topology.label(),
        role = ?config.role,
        node_id = %config.node_id,
        "starting WorldModelService"
    );

    let mut service = WorldModelService::new(config)
        .map_err(|e| anyhow::anyhow!("invalid config: {e}"))?;
    let report = service
        .boot()
        .map_err(|e| anyhow::anyhow!("boot failed: {e}"))?;

    tracing::info!(
        topology = ?report.topology,
        role = ?report.role,
        node_id = %report.node_id,
        latent_dim = report.latent_dim,
        available = report.available,
        "WorldModelService booted"
    );

    if cli.smoke_frames > 0 {
        let frames = service
            .run_fake_sensor_pipeline(cli.smoke_frames, cli.smoke_hash)
            .map_err(|e| anyhow::anyhow!("smoke pipeline: {e}"))?;
        tracing::info!(
            frames = frames.len(),
            chain_entries = service.chain.len(),
            "fake sensor pipeline complete"
        );
        for fr in &frames {
            tracing::debug!(
                frame_seq = fr.frame_seq,
                chain_seq = fr.chain_seq,
                surprise = fr.surprise,
                "attested frame"
            );
        }
    }

    // Single-node scaffold exits after smoke (or immediately if smoke_frames=0).
    // Long-running listen loop lands with LatticeApi ServiceApi (WEFT-527).
    if cli.smoke_frames == 0 {
        tracing::info!(
            "boot-only mode (no smoke frames); use --smoke-frames N for end-to-end check"
        );
    }

    Ok(())
}

fn parse_role(s: &str) -> anyhow::Result<ServiceRole> {
    match s.trim().to_ascii_lowercase().as_str() {
        "primary" => Ok(ServiceRole::Primary),
        "standby" => Ok(ServiceRole::Standby),
        "peer" => Ok(ServiceRole::Peer),
        other => anyhow::bail!("unknown role {other:?}; expected primary | standby | peer"),
    }
}
