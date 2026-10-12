//! `weaver mesh ...`: run the machine mesh service and administer it
//! (ADR-103, Phase 3 package S).
//!
//! `serve` runs the service in the foreground and refuses to run as root.
//! Every other verb is a mesh-local client: it connects to the service socket,
//! verifies the server (uid, machine key against `service.json` and the pin),
//! and sends one verb. Admin verbs connect as `role: "admin"` and hold no
//! keys; the service authorises them by peer credential (root or `admin_uids`).

mod admin;
mod client;
mod render;

pub(crate) use client::{socket_of, status_for_doctor};

use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use clawft_mesh_local::hexser;
use clawft_mesh_local::proto::{Message, Role};
use clawft_mesh_service::{MeshServiceConfig, Overrides};

const BUILD_VERSION: &str = env!("BUILD_VERSION");

/// `weaver mesh`.
#[derive(Args, Debug)]
pub struct MeshArgs {
    #[command(subcommand)]
    pub cmd: MeshCmd,
}

/// Where the service is and how to present the result.
#[derive(Args, Debug, Clone, Default)]
pub struct ConnArgs {
    /// Mesh-local socket (default: $WEFTOS_MESH_SOCKET, else /var/run/weftos/mesh.sock).
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Machine-key pin to compare against (default: ~/.weftos/mesh/machine.pub when present).
    #[arg(long)]
    pub pin: Option<PathBuf>,
    /// Print the raw JSON reply.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// mesh.toml (production: /etc/weftos/mesh.toml).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// State directory (default /var/lib/weftos/mesh; tests: a tempdir).
    #[arg(long)]
    pub state_dir: Option<PathBuf>,
    /// mesh-local socket path.
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Mesh listener address (default 127.0.0.1:9489; 0.0.0.0 exposes it to the LAN).
    #[arg(long)]
    pub listen: Option<String>,
    /// Loopback health address, or `off` (default 127.0.0.1:9490).
    #[arg(long)]
    pub health_listen: Option<String>,
}

#[derive(Args, Debug)]
pub struct TrustArgs {
    #[command(flatten)]
    pub conn: ConnArgs,
    /// Replace an existing pin that holds a different key.
    #[arg(long)]
    pub replace: bool,
}

#[derive(Args, Debug)]
pub struct IdentityArgs {
    /// Also write the raw 32-byte Ed25519 public key here (for
    /// `weaver leaf provision --mesh-pubkey`). Refuses to overwrite.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Print JSON (`report.mesh_identity` shape) instead of text.
    #[arg(long)]
    pub json: bool,
    /// The user key (`~/.weftos/user.key`, else the migrated chain key)
    /// instead of the node key: its key id is the `user:<id>` scope and
    /// `--out` writes the raw public key that `weaver leaf enroll
    /// --parent-pubkey` pins.
    #[arg(long)]
    pub user: bool,
}

#[derive(Subcommand, Debug)]
pub enum MeshCmd {
    /// Run the machine mesh service in the foreground (never as root).
    Serve(ServeArgs),
    /// Service status: identity, policy, registrations, peers, journal.
    Status(ConnArgs),
    /// List bound, pending and revoked users (admin).
    Bindings(ConnArgs),
    /// Approve, revoke or rebind a user binding (admin).
    Bind {
        #[command(subcommand)]
        cmd: BindCmd,
    },
    /// Revoke or restore a mesh peer (admin).
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Verify the machine journal; acknowledge a quarantined tail (admin).
    Journal {
        #[command(subcommand)]
        cmd: JournalCmd,
    },
    /// Pin the service's machine key after verifying its fingerprint out of band.
    Trust(TrustArgs),
    /// Print this node's mesh identity — node id, fingerprint and Ed25519
    /// public key — from `<runtime>/node.key`. Read-only; the private key is
    /// never printed. `--out` also writes the raw 32-byte public key, which
    /// is what `weaver leaf provision --mesh-pubkey` pins.
    Identity(IdentityArgs),
    /// Mint this mesh's MeshId (the `weftos://` authority) on its primary when
    /// there is no Seed licence; refuses if the mesh already has one.
    Create,
    /// Confirm this node uses its Seed licence's MeshId; refuses a conflicting record.
    AdoptLicence,
    /// Print this node's MeshId and where it came from (licence, genesis, learned).
    Id(crate::commands::mesh_id_cmd::IdArgs),
    /// Mesh nonce for the Seed licence mesh id (ADR-106); runs locally.
    Nonce {
        #[command(subcommand)]
        cmd: crate::commands::mesh_nonce::NonceCmd,
    },
    /// Ask to pair this node with another for project work (ADR-108 P2b); the
    /// dashboard approves.
    #[cfg(feature = "placement")]
    Pair {
        #[command(subcommand)]
        cmd: crate::commands::mesh_pair_cmd::PairCmd,
    },
    /// PRINT a reviewed shell script that installs the service (runs nothing).
    InstallService(crate::commands::mesh_install::InstallArgs),
    /// PRINT the inverse script (keeps node.key unless --purge-key; runs nothing).
    UninstallService(crate::commands::mesh_install::InstallArgs),
}

#[derive(Subcommand, Debug)]
pub enum BindCmd {
    /// Approve a bind waiting for approval. The key is named by its user id
    /// (fingerprint): by default the one pending now, shown before approving;
    /// the service refuses if the pending key is a different one.
    Approve {
        uid: u32,
        /// The pending key's user id, as shown by `weaver mesh bindings`.
        #[arg(long)]
        user_id: Option<String>,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Revoke the user's binding and every certificate issued to its key.
    Revoke {
        uid: u32,
        #[arg(long, default_value = "")]
        reason: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Replace the user's key (default: the key the uid last offered in conflict).
    Rebind {
        uid: u32,
        /// New user public key (64 hex).
        #[arg(long)]
        pubkey: Option<String>,
        #[command(flatten)]
        conn: ConnArgs,
    },
}

#[derive(Subcommand, Debug)]
pub enum PeerCmd {
    /// Refuse and disconnect a peer node id.
    Revoke {
        node_id: String,
        #[arg(long, default_value = "")]
        reason: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Remove a peer revocation.
    Unrevoke {
        node_id: String,
        #[command(flatten)]
        conn: ConnArgs,
    },
}

#[derive(Subcommand, Debug)]
pub enum JournalCmd {
    /// Re-verify the hash chain and signatures; fails when it does not verify.
    Verify {
        /// Acknowledge a quarantined tail (journals what was lost; lifts read-only).
        #[arg(long)]
        accept_truncate: bool,
        /// Quarantine record to accept (default: the newest pending one).
        #[arg(long)]
        seq: Option<u64>,
        /// Raise the certificate serial floor (never lowers it).
        #[arg(long)]
        floor: Option<u64>,
        #[command(flatten)]
        conn: ConnArgs,
    },
}

/// Entry point from `main`.
pub async fn run(args: MeshArgs) -> Result<()> {
    execute(args.cmd, &mut std::io::stdout()).await
}

/// Run one command, writing its output to `w` (tests capture it).
pub async fn execute(cmd: MeshCmd, w: &mut dyn Write) -> Result<()> {
    match cmd {
        MeshCmd::Serve(a) => serve(a).await,
        MeshCmd::Status(c) => {
            let data = client::call_best(&c, Message::Status {}).await?;
            client::show(w, &c, &data, render::render_status)
        }
        MeshCmd::Bindings(c) => {
            let data = client::call(&c, Role::Admin, Message::BindingsList {}).await?;
            client::show(w, &c, &data, render::render_bindings)
        }
        MeshCmd::Bind { cmd } => match cmd {
            BindCmd::Approve { uid, user_id, conn } => {
                let user_id = match user_id {
                    Some(u) => u,
                    None => client::pending_user_id(&conn, uid).await?,
                };
                writeln!(w, "approving uid {uid} with the key whose user id is {user_id}")?;
                client::ack(w, &conn, Message::BindApprove { uid, user_id: Some(user_id) }, &format!("approved uid {uid}")).await
            }
            BindCmd::Revoke { uid, reason, conn } => {
                client::ack(w, &conn, Message::BindRevoke { uid, reason }, &format!("revoked uid {uid}")).await
            }
            BindCmd::Rebind { uid, pubkey, conn } => {
                if let Some(k) = &pubkey
                    && hexser::decode::<32>(k).is_none()
                {
                    bail!("--pubkey must be 64 lowercase hex characters");
                }
                client::ack(w, &conn, Message::BindRebind { uid, user_pubkey: pubkey }, &format!("rebound uid {uid}")).await
            }
        },
        MeshCmd::Peer { cmd } => match cmd {
            PeerCmd::Revoke { node_id, reason, conn } => {
                let done = format!(
                    "revoked peer {node_id}; its live connection is closed. Under admission = enforce it is refused \
                     when it reconnects; under observe (the default) the revocation is only recorded and it can reconnect"
                );
                client::ack(w, &conn, Message::PeerRevoke { node_id, reason }, &done).await
            }
            PeerCmd::Unrevoke { node_id, conn } => {
                let done = format!("restored peer {node_id}");
                client::ack(w, &conn, Message::PeerUnrevoke { node_id }, &done).await
            }
        },
        MeshCmd::Journal { cmd: JournalCmd::Verify { accept_truncate, seq, floor, conn } } => {
            admin::journal_verify(w, &conn, accept_truncate, seq, floor).await
        }
        MeshCmd::Trust(t) => admin::trust(w, t).await,
        MeshCmd::Identity(a) => admin::identity(w, &a),
        MeshCmd::Create => crate::commands::mesh_id_cmd::create(w).await,
        MeshCmd::AdoptLicence => crate::commands::mesh_id_cmd::adopt_licence(w).await,
        MeshCmd::Id(a) => crate::commands::mesh_id_cmd::id(w, &a).await,
        MeshCmd::Nonce { cmd } => crate::commands::mesh_nonce::run(cmd, w),
        #[cfg(feature = "placement")]
        MeshCmd::Pair { cmd } => crate::commands::mesh_pair_cmd::run(cmd, w).await,
        MeshCmd::InstallService(a) => crate::commands::mesh_install::run_install(&a, w),
        MeshCmd::UninstallService(a) => crate::commands::mesh_install::run_uninstall(&a, w),
    }
}

async fn serve(a: ServeArgs) -> Result<()> {
    let overrides = Overrides { state_dir: a.state_dir, socket: a.socket, listen: a.listen };
    let mut cfg = MeshServiceConfig::load(a.config.as_deref(), &overrides).context("mesh service configuration")?;
    cfg.build_sha = BUILD_VERSION.to_string();
    if let Some(h) = a.health_listen {
        cfg.health_listen = if h == "off" { None } else { Some(h) };
        cfg.validate()?;
    }
    clawft_mesh_service::run(cfg).await.map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct W {
        #[command(subcommand)]
        cmd: MeshCmd,
    }

    fn parse(args: &[&str]) -> MeshCmd {
        W::try_parse_from(std::iter::once("mesh").chain(args.iter().copied())).unwrap().cmd
    }

    #[test]
    fn verbs_parse() {
        assert!(matches!(parse(&["serve", "--state-dir", "/x", "--listen", "127.0.0.1:19489"]), MeshCmd::Serve(_)));
        assert!(matches!(parse(&["status", "--json"]), MeshCmd::Status(c) if c.json));
        assert!(matches!(parse(&["bind", "approve", "501"]), MeshCmd::Bind { cmd: BindCmd::Approve { uid: 501, .. } }));
        assert!(matches!(
            parse(&["bind", "revoke", "501", "--reason", "gone"]),
            MeshCmd::Bind { cmd: BindCmd::Revoke { uid: 501, ref reason, .. } } if reason == "gone"
        ));
        assert!(matches!(parse(&["bind", "rebind", "501", "--pubkey", "ab"]), MeshCmd::Bind { cmd: BindCmd::Rebind { .. } }));
        assert!(matches!(parse(&["peer", "unrevoke", "abc"]), MeshCmd::Peer { cmd: PeerCmd::Unrevoke { .. } }));
        assert!(matches!(
            parse(&["journal", "verify", "--accept-truncate", "--floor", "9"]),
            MeshCmd::Journal { cmd: JournalCmd::Verify { accept_truncate: true, floor: Some(9), .. } }
        ));
        assert!(matches!(parse(&["trust", "--replace"]), MeshCmd::Trust(t) if t.replace));
    }
}
