//! Connection helpers for `weaver mesh` verbs: socket and pin resolution,
//! the mesh-local client calls, and output dispatch.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clawft_mesh_local::proto::{ErrorKind, Message, Role, ServiceRecord};
use clawft_mesh_service::admin_client::{AdminClient, AdminError, ConnectConfig};
use clawft_mesh_service::config::{DEFAULT_SOCKET, ENV_SOCKET};
use serde_json::Value;

use super::{ConnArgs, BUILD_VERSION};

pub(crate) fn socket_of(c: &ConnArgs) -> PathBuf {
    c.socket
        .clone()
        .or_else(|| std::env::var_os(ENV_SOCKET).map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET))
}

pub(super) fn default_pin() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".weftos/mesh/machine.pub"))
}

pub(super) fn record_of(socket: &std::path::Path) -> Result<ServiceRecord> {
    let path = socket.parent().map(|d| d.join("service.json")).context("socket has no directory")?;
    ServiceRecord::load(&path).with_context(|| {
        format!("cannot read {} (is the mesh service running? `weaver mesh serve` or the installed unit)", path.display())
    })
}

pub(super) async fn connect(c: &ConnArgs, role: Role, pin: bool) -> Result<AdminClient, AdminError> {
    let socket = socket_of(c);
    let service = record_of(&socket).map_err(|e| AdminError::Pin(format!("{e:#}")))?;
    let mut cc = ConnectConfig::new(socket, service, role);
    cc.build_sha = BUILD_VERSION.to_string();
    if pin {
        cc.pin = c.pin.clone().or_else(default_pin);
    }
    AdminClient::connect(&cc).await
}

pub(super) async fn call(c: &ConnArgs, role: Role, m: Message) -> Result<Value> {
    let mut cl = connect(c, role, true).await?;
    let reply = cl.request(m).await?;
    cl.bye().await;
    match reply {
        Message::Reply { data } => Ok(data),
        Message::Ack {} => Ok(Value::Null),
        other => bail!("unexpected reply: {other:?}"),
    }
}

/// Admin when the caller is one (full detail), else the plain user view.
pub(super) async fn call_best(c: &ConnArgs, m: Message) -> Result<Value> {
    match call(c, Role::Admin, m.clone()).await {
        Err(e) if matches!(e.downcast_ref::<AdminError>(), Some(AdminError::Server(b)) if b.kind == ErrorKind::Forbidden) => {
            call(c, Role::User, m).await
        }
        other => other,
    }
}

/// Status (best role) and, when this caller is an admin, the journal check;
/// read-only, for `weaver doctor`.
pub(crate) async fn status_for_doctor(c: &ConnArgs) -> Result<(Value, Option<Value>)> {
    let status = call_best(c, Message::Status {}).await?;
    let journal = call(c, Role::Admin, Message::JournalVerify {}).await.ok();
    Ok((status, journal))
}

/// The user id of the key pending approval for `uid`.
pub(super) async fn pending_user_id(c: &ConnArgs, uid: u32) -> Result<String> {
    let data = call(c, Role::Admin, Message::BindingsList {}).await?;
    data["pending"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["principal"]["uid"].as_u64() == Some(u64::from(uid)))
        .and_then(|p| p["user_id"].as_str().map(str::to_string))
        .with_context(|| format!("nothing is pending approval for uid {uid}"))
}

pub(super) async fn ack(w: &mut dyn Write, c: &ConnArgs, m: Message, done: &str) -> Result<()> {
    let data = call(c, Role::Admin, m).await?;
    writeln!(w, "{done}")?;
    if let Some(warning) = data["warning"].as_str() {
        writeln!(w, "WARNING: {warning}")?;
    }
    Ok(())
}

pub(super) fn show(w: &mut dyn Write, c: &ConnArgs, data: &Value, render: fn(&mut dyn Write, &Value) -> Result<()>) -> Result<()> {
    if c.json {
        writeln!(w, "{}", serde_json::to_string_pretty(data)?)?;
        Ok(())
    } else {
        render(w, data)
    }
}
