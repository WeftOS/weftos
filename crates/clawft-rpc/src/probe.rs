//! Socket diagnosis: say which endpoint was tried and why it is unusable.
//!
//! The CLI must never fail with a bare "no daemon running" (ADR-103 D14).
//! [`probe_socket`] classifies the endpoint and [`describe_unreachable`]
//! renders the operator-facing message, naming the path, the resolved
//! runtime root and where that root came from.

use std::path::Path;

use clawft_types::runtime_paths::{RootSource, RuntimePaths};

/// Result of dialing a kernel endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketState {
    /// A daemon accepted the connection.
    Reachable,
    /// No socket file (Unix) or no pipe server (Windows) at the path.
    NoSocketFile,
    /// The socket file exists but nothing accepts on it (previous daemon
    /// died without unlinking it).
    Stale,
    /// The socket exists but this user may not connect to it.
    PermissionDenied,
    /// Any other dial failure (message from the OS).
    Other(String),
}

/// Dial `path` once and classify the outcome.
#[cfg(unix)]
pub async fn probe_socket(path: &Path) -> SocketState {
    use std::io::ErrorKind;
    match tokio::net::UnixStream::connect(path).await {
        Ok(_) => SocketState::Reachable,
        Err(e) => match e.kind() {
            ErrorKind::NotFound => SocketState::NoSocketFile,
            ErrorKind::ConnectionRefused => SocketState::Stale,
            ErrorKind::PermissionDenied => SocketState::PermissionDenied,
            // A path over `sun_path` fails with a bare "path must be shorter
            // than SUN_LEN"; say which path and by how much.
            _ => match clawft_types::socket_path::check(path) {
                Err(t) => SocketState::Other(format!(
                    "the socket path is {} bytes, over the platform limit of {} bytes; \
                     use a shorter runtime directory (WEFTOS_RUNTIME_DIR)",
                    t.bytes,
                    t.max_bytes()
                )),
                Ok(()) => SocketState::Other(e.to_string()),
            },
        },
    }
}

/// Dial the pipe derived from `path` once and classify the outcome.
#[cfg(not(unix))]
pub async fn probe_socket(path: &Path) -> SocketState {
    if crate::DaemonClient::connect_path(path).await.is_some() {
        SocketState::Reachable
    } else {
        SocketState::NoSocketFile
    }
}

/// One-clause reason an endpoint is unusable (no trailing guidance).
pub fn describe_state(path: &Path, state: &SocketState) -> String {
    let endpoint = if cfg!(windows) {
        format!(
            "{} (named pipe {})",
            path.display(),
            crate::pipe_name_for_path(path)
        )
    } else {
        path.display().to_string()
    };
    match state {
        SocketState::Reachable => "reachable".to_string(),
        SocketState::NoSocketFile => {
            format!("no socket file at {endpoint}; no kernel is running there")
        }
        SocketState::Stale => format!(
            "stale socket at {endpoint}: the file exists but connection was refused \
             (the previous daemon died; starting a kernel here will replace it)"
        ),
        SocketState::PermissionDenied => {
            format!("permission denied connecting to {endpoint}; it belongs to another user")
        }
        SocketState::Other(e) => format!("cannot connect to {endpoint}: {e}"),
    }
}

/// Operator-facing explanation for an unreachable endpoint.
pub fn describe_unreachable(path: &Path, state: &SocketState, paths: &RuntimePaths) -> String {
    let why = describe_state(path, state);
    let origin = match paths.source() {
        RootSource::Env => "WEFTOS_RUNTIME_DIR".to_string(),
        RootSource::Project(p) => format!("project {}", p.display()),
        RootSource::LegacyHome => "legacy ~/.clawft (no project found from here)".to_string(),
        RootSource::User => "user daemon root ~/.weftos/run".to_string(),
        RootSource::Child { id, .. } => format!("project kernel {id} under ~/.weftos/run"),
    };
    format!(
        "no kernel reachable: {why}\n  runtime root: {} (from {origin})\n  \
         start one with `weaver kernel start`, or point at another with WEFTOS_RUNTIME_DIR",
        paths.root().display()
    )
}

impl SocketState {
    /// One-phrase reason for single-line warnings.
    pub fn short_reason(&self) -> String {
        match self {
            SocketState::Reachable => "reachable".into(),
            SocketState::NoSocketFile => "no socket file".into(),
            SocketState::Stale => "stale socket, connection refused".into(),
            SocketState::PermissionDenied => "permission denied".into(),
            SocketState::Other(e) => e.clone(),
        }
    }
}

/// Probe the default endpoint: the socket path tried and what was found.
pub async fn probe_default() -> (std::path::PathBuf, SocketState) {
    let sock = RuntimePaths::resolve().socket();
    let state = probe_socket(&sock).await;
    (sock, state)
}

/// Probe the default endpoint and, if it is unusable, describe why.
pub async fn unreachable_message() -> String {
    let paths = RuntimePaths::resolve();
    let sock = paths.socket();
    let state = probe_socket(&sock).await;
    describe_unreachable(&sock, &state, &paths)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn classifies_missing_stale_and_live() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("kernel.sock");
        assert_eq!(probe_socket(&sock).await, SocketState::NoSocketFile);

        // Bind then drop the listener: the file stays, nothing accepts.
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        drop(listener);
        assert!(sock.exists());
        assert_eq!(probe_socket(&sock).await, SocketState::Stale);

        std::fs::remove_file(&sock).unwrap();
        let _live = tokio::net::UnixListener::bind(&sock).unwrap();
        assert_eq!(probe_socket(&sock).await, SocketState::Reachable);
    }

    #[tokio::test]
    async fn an_over_long_socket_path_says_so_and_names_the_path() {
        use clawft_types::socket_path::{PLATFORM_LIMIT, max_path_bytes};
        let dir = tempfile::tempdir().unwrap();
        let mut root = dir.path().to_path_buf();
        while root.as_os_str().len() + "/kernel.sock".len() <= max_path_bytes(PLATFORM_LIMIT) {
            root.push("d".repeat(30));
        }
        let sock = root.join("kernel.sock");
        let bytes = sock.as_os_str().len();
        let state = probe_socket(&sock).await;
        let SocketState::Other(why) = &state else { panic!("{state:?}") };
        assert!(why.contains(&format!("{bytes} bytes")), "{why}");
        assert!(why.contains(&format!("{} bytes", max_path_bytes(PLATFORM_LIMIT))), "{why}");
        assert!(!why.contains("SUN_LEN"), "{why}");
        let paths = RuntimePaths::at(&root);
        let msg = describe_unreachable(&sock, &state, &paths);
        assert!(msg.contains(&sock.display().to_string()), "{msg}");
        assert!(msg.contains("shorter runtime directory"), "{msg}");
    }

    #[test]
    fn message_names_socket_state_and_root() {
        let paths = RuntimePaths::at("/run/x");
        let msg = describe_unreachable(&paths.socket(), &SocketState::Stale, &paths);
        assert!(msg.contains("/run/x/kernel.sock"), "{msg}");
        assert!(msg.contains("stale"), "{msg}");
        assert!(msg.contains("WEFTOS_RUNTIME_DIR"), "{msg}");
        let msg = describe_unreachable(&paths.socket(), &SocketState::PermissionDenied, &paths);
        assert!(msg.contains("permission denied"), "{msg}");
        let msg = describe_unreachable(&paths.socket(), &SocketState::NoSocketFile, &paths);
        assert!(msg.contains("no socket file"), "{msg}");
    }
}
