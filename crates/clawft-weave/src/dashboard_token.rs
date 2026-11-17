//! Dashboard token rotation (`dashboard.token.rotate`).
//!
//! The daemon presents its current node token to
//! `POST /api/nodes/token/rotate` with `{"node_id"}`; the dashboard issues a
//! new token and revokes the presented one atomically. The new token is then
//! written with a temp file (mode 0600, same directory) and a rename, and only
//! after that does the call report success.
//!
//! If the write fails after the dashboard rotated, the old token is already
//! revoked. The new token is then kept in memory (used for heartbeats, written
//! on a later beat once the problem is fixed), the failure is logged loudly
//! with the remedy, and the call reports an error. No token is ever logged or
//! returned.

use std::io::Write;
use std::path::Path;

use serde_json::{Value, json};

use crate::dashboard_report::Dashboard;

/// `wft_` followed by 64 hex characters, the shape the dashboard issues.
pub fn is_issued_token(t: &str) -> bool {
    t.strip_prefix("wft_").is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Write `contents` to `path` atomically with mode 0600: a uniquely named temp
/// file in the same directory (created exclusively), fsync, rename, fsync of
/// the directory.
#[cfg(unix)]
pub fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().ok_or_else(|| std::io::Error::other("token path has no directory"))?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("token");
    let suffix: u64 = rand::random();
    let tmp = dir.join(format!(".{name}.{}.{suffix:016x}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        // The umask may have narrowed the mode; make it exactly 0600.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(contents.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Non-unix platforms are refused at config time; this keeps the build whole.
#[cfg(not(unix))]
pub fn write_atomic(_path: &Path, _contents: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("the dashboard reporter needs a unix host"))
}

impl Dashboard {
    /// Rotate the node token. See the module docs.
    pub async fn rotate(&self) -> Result<Value, String> {
        let _guard = self.token_lock.lock().await;
        let result = self.rotate_locked().await;
        let text = match &result {
            Ok(_) => "ok".to_owned(),
            Err(e) => e.clone(),
        };
        self.with_state(|s| {
            s.last_rotation_at = Some(chrono::Utc::now().to_rfc3339());
            s.last_rotation = Some(text);
        });
        result
    }

    async fn rotate_locked(&self) -> Result<Value, String> {
        let path = self.cfg.token_path()?.to_path_buf();
        // A token that rotated but was never written is the live one.
        let had_pending = self.pending.lock().expect("pending lock poisoned").is_some();
        let current = self.current_token()?;
        let (status, text) = self
            .post("/api/nodes/token/rotate", &current, &json!({ "node_id": self.cfg.node_id }))
            .await
            .map_err(|e| format!("could not reach the dashboard: {e}"))?;
        match status {
            200..=299 => {}
            401 | 403 => {
                return Err(format!(
                    "dashboard token rejected ({status}): the current token is invalid or already revoked, so it \
                     cannot be rotated; issue a new token in the dashboard and write it to {}",
                    path.display()
                ));
            }
            s => return Err(format!("dashboard answered HTTP {s} to the rotate request; the token is unchanged")),
        }
        let v: Value = serde_json::from_str(&text).map_err(|_| unusable(&path))?;
        let new = v.get("token").and_then(Value::as_str).filter(|t| is_issued_token(t)).ok_or_else(|| unusable(&path))?;
        let rotated_at = v.get("rotated_at").cloned().unwrap_or(Value::Null);
        match write_atomic(&path, new) {
            Ok(()) => {
                *self.pending.lock().expect("pending lock poisoned") = None;
                tracing::info!(token_file = %path.display(), had_pending, "dashboard token rotated and saved");
                Ok(json!({ "rotated": true, "rotated_at": rotated_at, "token_file": path }))
            }
            Err(e) => {
                *self.pending.lock().expect("pending lock poisoned") = Some(new.to_owned());
                let msg = format!(
                    "the dashboard rotated this node's token but it could NOT be written to {}: {e}. The previous \
                     token is already revoked. The new token is held in memory and written on a later heartbeat \
                     once the file is writable; if the daemon restarts first, issue a new token in the dashboard \
                     and write it to that file (mode 600)",
                    path.display()
                );
                tracing::error!("{msg}");
                Err(msg)
            }
        }
    }

    /// Write a rotated token that could not be saved earlier (called each beat).
    pub async fn persist_pending(&self) {
        let _guard = self.token_lock.lock().await;
        let Some(t) = self.pending.lock().expect("pending lock poisoned").clone() else { return };
        let Ok(path) = self.cfg.token_path() else { return };
        match write_atomic(path, &t) {
            Ok(()) => {
                *self.pending.lock().expect("pending lock poisoned") = None;
                tracing::info!(token_file = %path.display(), "the rotated dashboard token is now saved");
            }
            Err(e) => tracing::error!(
                token_file = %path.display(), error = %e,
                "the rotated dashboard token is still not saved (the previous token is revoked)"
            ),
        }
    }
}

fn unusable(path: &Path) -> String {
    format!(
        "the dashboard rotated the token but its answer held no usable token, so the old one is revoked and \
         nothing was saved; issue a new token in the dashboard and write it to {}",
        path.display()
    )
}
