//! The per-user daemon profile (ADR-103 Phase 1, package D).
//!
//! `weaver kernel start --profile user` runs the collapsed `machine` + `user`
//! roles with runtime root `~/.weftos/run` ([`RootSource::User`]), one per
//! uid (the `kernel.lock` in that root). Configuration comes from
//! `~/.weftos/weave.toml` layered over the legacy `~/.clawft/config.json`
//! (plan D-2). The default `kernel start` behaviour is unchanged.
//!
//! The profile is process state, set once by the CLI ([`enter`]) so the
//! socket, pid, boot and chain choice all resolve the same root.
//!
//! [`RootSource::User`]: clawft_types::runtime_paths::RootSource::User

use std::path::{Path, PathBuf};

use clawft_platform::{Platform, config_loader};
use clawft_types::config::Config;
use clawft_types::runtime_paths::{
    absolutize, home_dir, set_user_profile, user_profile_active, user_weftos_dir,
};

use crate::commands::LoadedConfig;

/// Profile name accepted by `--profile` / `WEAVER_PROFILE`.
pub const PROFILE_USER: &str = "user";

/// Profile of a per-project child kernel (`--profile project`, ADR-103 A6).
/// Started by the user daemon only; see `project_boot`.
pub const PROFILE_PROJECT: &str = "project";

/// Roles the user daemon runs (collapsed machine + user, ADR-103 roles table).
pub const USER_ROLES: [&str; 2] = ["machine", "user"];

static PROJECT_PROFILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record that this process was started with `--profile project` (or
/// `WEAVER_PROFILE=project`); `daemon::run` then forces
/// `KernelProfile::Project` so `pre_boot` runs the child handshake.
pub fn set_project_profile(on: bool) {
    PROJECT_PROFILE.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// True when `--profile project` was requested for this process.
pub fn project_profile_requested() -> bool {
    PROJECT_PROFILE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Roles of the user daemon when the machine role lives in the mesh service.
pub const SERVICE_MODE_ROLES: [&str; 1] = ["user"];

/// Parse a `--profile` value. `None` and `default` mean the existing
/// project/legacy behaviour.
pub fn parse_profile(value: Option<&str>) -> Result<Option<&'static str>, String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None | Some("default") => Ok(None),
        Some(PROFILE_USER) => Ok(Some(PROFILE_USER)),
        Some(PROFILE_PROJECT) => Ok(Some(PROFILE_PROJECT)),
        Some(other) => Err(format!(
            "unknown profile {other:?}: expected `user` or `project` (or omit for the default)"
        )),
    }
}

/// Activate the user profile for this process: every later
/// `RuntimePaths::resolve()` returns the user root. Must run before the
/// working directory changes ([`prepare_home`]); it captures a relative
/// `WEFTOS_RUNTIME_DIR` as an absolute path.
pub fn enter() {
    set_user_profile(true);
}

/// [`enter`] with an explicit run root instead of `$WEFTOS_RUNTIME_DIR`
/// (tests that host the user daemon in process).
pub fn enter_at(run_root: &Path) {
    clawft_types::runtime_paths::set_user_profile_at(run_root);
}

/// Leave the user profile (tests).
pub fn leave() {
    set_user_profile(false);
}

/// True when this process runs or addresses the user daemon.
pub fn is_active() -> bool {
    user_profile_active()
}

/// A `--config` path made absolute, so the later `chdir` to `~/.weftos`
/// cannot change what it names.
pub fn absolutize_config(config: Option<&str>) -> Option<String> {
    config.map(|c| absolutize(Path::new(c)).to_string_lossy().into_owned())
}

/// Profile and roles to report in the handshake (`None` for the default daemon).
pub fn handshake_profile() -> Option<(String, Vec<String>)> {
    is_active().then(|| {
        (
            PROFILE_USER.to_owned(),
            roles_for(crate::mesh_state::global()),
        )
    })
}

/// Roles for the handshake: the machine role belongs to the mesh service when
/// this daemon is its client.
pub fn roles_for(mesh: &crate::mesh_state::MeshStateCell) -> Vec<String> {
    let roles: &[&str] = if mesh.is_service() { &SERVICE_MODE_ROLES } else { &USER_ROLES };
    roles.iter().map(|r| (*r).to_owned()).collect()
}

/// `<home>/.weftos/weave.toml`.
pub fn user_weave_toml(home: &Path) -> PathBuf {
    user_weftos_dir(home).join("weave.toml")
}

/// `<home>/.weftos/projects`, the manifest store.
pub fn manifests_dir(home: &Path) -> PathBuf {
    clawft_rpc::resolve::manifests_dir(home)
}

/// Parent-owned endpoint directory contains only the child RPC socket so it
/// can be mounted read-only as one stable directory inside a container.
pub fn child_socket_dir(run_root: &Path) -> PathBuf {
    run_root.join("child-ipc")
}

pub fn child_socket_path(run_root: &Path) -> PathBuf {
    child_socket_dir(run_root).join("child.sock")
}

/// Bind the production child-only endpoint under the daemon's held instance
/// lock. The caller retains the listener for the separate child accept loop.
/// Tests use this same binder with an isolated runtime root.
#[cfg(unix)]
pub fn bind_child_socket(run_root: &Path) -> anyhow::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};

    let dir = child_socket_dir(run_root);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if !meta.file_type().is_dir() || meta.file_type().is_symlink() => {
            anyhow::bail!("child socket directory is not a plain directory: {}", dir.display());
        }
        Ok(_) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&dir)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?,
        Err(e) => return Err(e.into()),
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let socket = child_socket_path(run_root);
    match std::fs::symlink_metadata(&socket) {
        Ok(meta) if meta.file_type().is_socket() || meta.file_type().is_symlink() => {
            std::fs::remove_file(&socket)?;
        }
        Ok(_) => anyhow::bail!("child socket path is not a socket: {}", socket.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e.into()),
    }
    tokio::net::UnixListener::bind(&socket)
        .map_err(|e| anyhow::anyhow!("cannot bind the child-only socket {}: {e}", socket.display()))
}

/// The local uid as a string (Unix), reported unverified in the handshake.
pub fn local_uid() -> Option<String> {
    #[cfg(unix)]
    {
        Some(nix::unistd::getuid().as_raw().to_string())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Create `~/.weftos` and the runtime root, and make `~/.weftos` the working
/// directory so no project-local `weave.toml` or `.clawft/` overlay leaks into
/// the daemon's config.
pub fn prepare_home(home: &Path) -> anyhow::Result<()> {
    let weftos = user_weftos_dir(home);
    std::fs::create_dir_all(weftos.join("run"))?;
    std::env::set_current_dir(&weftos)?;
    Ok(())
}

/// Layer `weave` (from `~/.weftos/weave.toml`) over the legacy layers.
///
/// Pure so it can be tested without a filesystem: the JSON layers are the
/// base, `weave` wins on conflict, the workspace overlay (if any) stays
/// split for the permission ceiling exactly as in `load_config_layered`.
pub fn layer_user_config(
    layers: config_loader::ConfigLayers,
    weave: &serde_json::Value,
) -> anyhow::Result<LoadedConfig> {
    let mut global_value = layers.global.clone();
    config_loader::deep_merge(&mut global_value, weave);
    let global: Config = serde_json::from_value(global_value.clone())?;
    let workspace_routing = match layers.workspace.clone() {
        Some(ws) => Some(serde_json::from_value::<Config>(ws)?.routing),
        None => None,
    };
    let merged = config_loader::ConfigLayers {
        global: global_value,
        workspace: layers.workspace,
    }
    .merged();
    Ok(LoadedConfig {
        config: serde_json::from_value(merged)?,
        global_routing: global.routing,
        workspace_routing,
    })
}

/// Load the user daemon's config. An explicit `--config` file is used as-is
/// (the same as every other profile); otherwise the legacy layers with
/// `~/.weftos/weave.toml` on top (D-2).
pub async fn load_user_config<P: Platform>(
    platform: &P,
    config_override: Option<&str>,
    home: &Path,
) -> anyhow::Result<LoadedConfig> {
    if config_override.is_some() {
        return crate::commands::load_config_layered(platform, config_override).await;
    }
    let layers = config_loader::load_config_layers(platform.fs(), platform.env())
        .await
        .map_err(|e| anyhow::anyhow!("failed to load config: {e}"))?;
    let path = user_weave_toml(home);
    let weave = config_loader::load_weave_toml_file(platform.fs(), &path)
        .await
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    layer_user_config(layers, &weave)
}

/// The home directory, or an error naming what is missing.
pub fn require_home() -> anyhow::Result<PathBuf> {
    home_dir().ok_or_else(|| {
        anyhow::anyhow!("cannot determine the home directory for --profile user")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn profile_flag_parsing() {
        assert_eq!(parse_profile(None), Ok(None));
        assert_eq!(parse_profile(Some("")), Ok(None));
        assert_eq!(parse_profile(Some("default")), Ok(None));
        assert_eq!(parse_profile(Some("user")), Ok(Some("user")));
        assert_eq!(parse_profile(Some("project")), Ok(Some("project")));
        assert!(parse_profile(Some("root")).unwrap_err().contains("unknown profile"));
    }

    #[test]
    fn weave_toml_wins_over_legacy_json_and_keeps_the_rest() {
        let layers = config_loader::ConfigLayers {
            global: json!({"kernel": {"max_processes": 7, "mesh": {"enabled": false}}}),
            workspace: None,
        };
        let weave = json!({"kernel": {"mesh": {"enabled": true, "listen_addr": "0.0.0.0:9470"}}});
        let loaded = layer_user_config(layers, &weave).unwrap();
        assert_eq!(loaded.config.kernel.max_processes, 7);
        let mesh = loaded.config.kernel.mesh.expect("mesh from weave.toml");
        assert!(mesh.enabled);
        assert_eq!(mesh.listen_addr, "0.0.0.0:9470");
    }

    #[tokio::test]
    async fn weave_toml_kernel_mesh_service_is_the_documented_key() {
        // The owner steps say: `service = "required"` under `[kernel.mesh]`.
        use clawft_platform::Platform;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("weave.toml");
        std::fs::write(&path, "[kernel.mesh]\nenabled = true\nservice = \"required\"\n").unwrap();
        let platform = clawft_platform::NativePlatform::new();
        let weave = config_loader::load_weave_toml_file(platform.fs(), &path).await.unwrap();
        let layers = config_loader::ConfigLayers { global: json!({}), workspace: None };
        let mesh = layer_user_config(layers, &weave).unwrap().config.kernel.mesh.expect("mesh");
        assert_eq!(mesh.service, clawft_types::config::MeshServicePolicy::Required);
    }

    #[test]
    fn empty_weave_toml_is_the_legacy_config() {
        let layers = config_loader::ConfigLayers {
            global: json!({"kernel": {"max_processes": 9}}),
            workspace: None,
        };
        let loaded = layer_user_config(layers, &json!({})).unwrap();
        assert_eq!(loaded.config.kernel.max_processes, 9);
        assert!(loaded.workspace_routing.is_none());
    }

    #[test]
    fn roles_are_user_only_in_service_mode_and_machine_user_otherwise() {
        let cell = crate::mesh_state::MeshStateCell::new();
        assert_eq!(roles_for(&cell), ["machine", "user"], "undecided");
        cell.set(crate::mesh_state::plain("collapsed"));
        assert_eq!(roles_for(&cell), ["machine", "user"], "collapsed");
        cell.set(clawft_rpc::handshake::MeshHandshake {
            mode: "service".into(),
            state: Some("connected".into()),
            ..Default::default()
        });
        assert_eq!(roles_for(&cell), ["user"], "service");
    }

    #[test]
    fn config_path_is_absolutized() {
        assert_eq!(absolutize_config(None), None);
        assert_eq!(absolutize_config(Some("/a/b.json")).as_deref(), Some("/a/b.json"));
        let abs = absolutize_config(Some("cfg.json")).unwrap();
        assert_eq!(
            Path::new(&abs),
            std::env::current_dir().unwrap().join("cfg.json")
        );
    }

    #[test]
    fn user_paths_hang_off_home_weftos() {
        let home = Path::new("/h");
        assert_eq!(user_weave_toml(home), Path::new("/h/.weftos/weave.toml"));
        assert_eq!(manifests_dir(home), Path::new("/h/.weftos/projects"));
    }
}
