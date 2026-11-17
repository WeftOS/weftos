//! `kernel.handshake` and the request-envelope gate (ADR-103 D14, package A).
//!
//! The handshake tells a client which daemon it reached (node, project,
//! runtime dir, build) so a stale socket pointing at the wrong daemon is
//! caught before any real call. [`envelope_refusal`] rejects requests whose
//! `proto` is outside the supported range, whose `project` is not a ULID,
//! or whose `project` differs from the project this daemon is bound to.
//! `kernel.handshake` itself is exempt: it is how a client discovers the
//! accepted range and the bound project.
//!
//! The bound project is decided once at startup ([`init_bound`]) from the
//! paths the daemon booted with, never re-resolved per call.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use clawft_rpc::handshake::{
    BoundVia, DaemonBuild, Handshake, INVALID_PROJECT_KIND, PROJECT_MISMATCH_KIND, ProtoCheck,
    ProtoRange, check_proto, handshake_value, proto_mismatch_response,
};
use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use clawft_types::project::{list_manifests, read_project_toml, validate_id};
use clawft_types::runtime_paths::{RootSource, RuntimePaths};

use crate::rpc_ext::{ExtCall, ExtFuture};

const BUILD_SHA: &str = env!("BUILD_GIT_HASH");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The discovery call, exempt from the envelope refusal.
const HANDSHAKE_METHOD: &str = "kernel.handshake";

/// The project a daemon is bound to, and how that was decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundProject {
    pub project_id: Option<String>,
    pub via: BoundVia,
}

static BOUND: RwLock<Option<BoundProject>> = RwLock::new(None);
/// Runtime root the daemon booted with (handshake `runtime_dir`).
static RUNTIME_ROOT: RwLock<Option<PathBuf>> = RwLock::new(None);

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Decide the bound project for a daemon owning `paths`.
///
/// - a spawned child kernel (`RootSource::Child`, ADR-103 A7): the project it
///   was started for, which `pre_boot` checked against `spawn.json`, the run
///   dir name and the certificate (`BoundVia::Project`);
/// - root resolved from a project directory: that `project.toml` id
///   (`BoundVia::Project`);
/// - otherwise: the one manifest in `manifests_dir` whose `[serve]
///   runtime_dir` canonicalizes to the root, if exactly one
///   (`BoundVia::Manifest`);
/// - else unbound.
pub fn compute_bound(paths: &RuntimePaths, manifests_dir: Option<&Path>) -> BoundProject {
    if let RootSource::Child { id, .. } = paths.source() {
        return BoundProject {
            project_id: Some(id.clone()),
            via: BoundVia::Project,
        };
    }
    if let RootSource::Project(dir) = paths.source() {
        if let Some(pt) = read_project_toml(dir).ok().flatten() {
            return BoundProject {
                project_id: Some(pt.id),
                via: BoundVia::Project,
            };
        }
        return BoundProject::default();
    }
    let Some(mdir) = manifests_dir else {
        return BoundProject::default();
    };
    let root = canon(paths.root());
    let matches: Vec<String> = list_manifests(mdir)
        .map(|l| l.manifests)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.runtime_dir_override()
                .is_some_and(|rt| rt.is_absolute() && canon(rt) == root)
        })
        .map(|m| m.id)
        .collect();
    match matches.as_slice() {
        [one] => BoundProject {
            project_id: Some(one.clone()),
            via: BoundVia::Manifest,
        },
        [] => BoundProject::default(),
        many => {
            tracing::warn!(
                runtime_dir = %root.display(),
                projects = ?many,
                "several project manifests claim this runtime_dir; daemon stays unbound"
            );
            BoundProject::default()
        }
    }
}

/// Record the bound project at daemon startup.
pub fn init_bound(paths: &RuntimePaths, manifests_dir: Option<&Path>) {
    let b = compute_bound(paths, manifests_dir);
    tracing::info!(project = ?b.project_id, via = ?b.via, "daemon project binding");
    *RUNTIME_ROOT.write().expect("RUNTIME_ROOT lock poisoned") = Some(paths.root().to_path_buf());
    set_bound(b);
}

/// Replace the recorded binding (startup and tests).
pub(crate) fn set_bound(b: BoundProject) {
    *BOUND.write().expect("BOUND lock poisoned") = Some(b);
}

/// The project id this daemon is bound to, if any (scope gate, ADR-103 D12).
pub fn bound_project_id() -> Option<String> {
    bound().project_id
}

pub(crate) fn bound() -> BoundProject {
    BOUND
        .read()
        .expect("BOUND lock poisoned")
        .clone()
        .unwrap_or_default()
}

/// Assemble the handshake for a daemon owning `paths`.
pub fn build_handshake(
    node_id: String,
    paths: &RuntimePaths,
    binary: Option<PathBuf>,
    bound: &BoundProject,
) -> Handshake {
    Handshake {
        proto: ProtoRange::supported(),
        node_id,
        user_id: None,
        user_key_id: None,
        profile: None,
        roles: Vec::new(),
        project_id: bound.project_id.clone(),
        bound_via: bound.via,
        depth: 0,
        parent: None,
        runtime_dir: paths.root().display().to_string(),
        pid: std::process::id(),
        version: VERSION.to_owned(),
        sha: BUILD_SHA.to_owned(),
        binary: binary.map(|p| p.display().to_string()),
        mesh: None,
    }
}

/// Apply the user-daemon fields: profile, roles, local uid and user key id.
/// A default (project/legacy) daemon leaves all four empty. Every daemon
/// reports its mesh mode (`collapsed`, `off`, or `service` with the link).
pub fn with_user_profile(mut h: Handshake, user_key_id: Option<String>) -> Handshake {
    if let Some((profile, roles)) = crate::user_daemon::handshake_profile() {
        h.profile = Some(profile);
        h.roles = roles;
        h.user_id = crate::user_daemon::local_uid();
        h.user_key_id = user_key_id;
    }
    #[cfg(all(unix, feature = "exochain"))]
    if let Some(nested) = crate::nested_boot::active() {
        h.depth = nested.instance.depth;
        h.parent = Some(nested.instance.parent.clone());
    }
    h.mesh = crate::mesh_state::global().get();
    h
}

/// Id of the user key: the node-id-style hash of the chain verifying key
/// (plan D-1; becomes `~/.weftos/user.key` in Phase 3). `None` for a daemon
/// that is not the user daemon, or has no chain signing key.
pub fn user_key_id(kernel: &Kernel<NativePlatform>) -> Option<String> {
    if !crate::user_daemon::is_active() {
        return None;
    }
    #[cfg(feature = "exochain")]
    {
        let vk = kernel.chain_manager()?.verifying_key()?;
        Some(clawft_kernel::node_id_from_pubkey(&vk.to_bytes()))
    }
    #[cfg(not(feature = "exochain"))]
    {
        let _ = kernel;
        None
    }
}

/// The handshake of this running daemon, from its booted state.
pub fn current_handshake(kernel: &Kernel<NativePlatform>) -> Handshake {
    let node_id = kernel.cluster_membership().local_node_id().to_owned();
    let h = build_handshake(
        node_id,
        &RUNTIME_ROOT
            .read()
            .expect("RUNTIME_ROOT lock poisoned")
            .clone()
            .map_or_else(RuntimePaths::resolve, RuntimePaths::at),
        std::env::current_exe().ok(),
        &bound(),
    );
    with_user_profile(h, user_key_id(kernel))
}

/// `kernel.handshake` handler (registered in `rpc_ext::ROUTES`, `Read`).
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        use ed25519_dalek::Signer;
        let kernel = call.ctx.kernel.read().await;
        let h = current_handshake(&kernel);
        let mut value = handshake_value(&h);
        if let (Some(id), Some(nonce)) = (h.project_id.as_deref(), call.params.get("challenge").and_then(|v| v.as_str()))
            && nonce.len() == 64 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
            && let Some(key) = kernel.chain_manager().and_then(|c| c.signing_key_clone())
        {
            let bytes = format!("weftos-project-handshake-v1\n{id}\n{nonce}");
            value["project_proof"] = serde_json::json!(hex::encode(key.sign(bytes.as_bytes()).to_bytes()));
        }
        Response::success(value)
    })
}

/// Refuse a request with an unsupported `proto`, a malformed `project`, or
/// a `project` other than the daemon's bound one. `kernel.handshake` is
/// exempt from the proto and mismatch refusals only; its claim must still
/// be a ULID (the `ClaimedProject` invariant).
///
/// A missing `proto` is a legacy client: accepted for read-only methods with
/// one warning per process, refused for every other method (0.8.2, ADR-103
/// A8).
pub fn envelope_refusal(method: &str, proto: Option<u32>, project: Option<&str>) -> Option<Response> {
    envelope_refusal_with(&bound(), method, proto, project)
}

/// [`envelope_refusal`] against an explicit binding.
pub fn envelope_refusal_with(
    bound: &BoundProject,
    method: &str,
    proto: Option<u32>,
    project: Option<&str>,
) -> Option<Response> {
    // The discovery call is exempt from the proto and mismatch refusals,
    // but its claimed project must still be well-formed.
    let discovery = method == HANDSHAKE_METHOD;
    if !discovery {
        match check_proto(proto) {
            ProtoCheck::Supported => {}
            ProtoCheck::Legacy => {
                // Explicit allowlist, shared with the client: a method the
                // table does not know defaults to Read there, so it must not
                // pass here on that basis.
                if !clawft_rpc::handshake::is_read_only_method(method) {
                    return Some(proto_mismatch_response(
                        0,
                        DaemonBuild {
                            sha: BUILD_SHA,
                            version: VERSION,
                        },
                    ));
                }
                warn_legacy_once();
            }
            ProtoCheck::Unsupported(p) => {
                return Some(proto_mismatch_response(
                    p,
                    DaemonBuild {
                        sha: BUILD_SHA,
                        version: VERSION,
                    },
                ));
            }
        }
    }
    let id = project?;
    if validate_id(id).is_err() {
        return Some(Response::error_with_kind(
            INVALID_PROJECT_KIND,
            format!("invalid project id {id:?}: expected a 26-character ULID"),
        ));
    }
    if !discovery
        && let Some(mine) = &bound.project_id
        && mine != id
    {
        return Some(Response::error_with_kind(
            PROJECT_MISMATCH_KIND,
            format!(
                "this daemon serves project {mine}, not {id}; \
                 unset WEFTOS_RUNTIME_DIR or pass `--project {id}` to reach its own kernel"
            ),
        ));
    }
    None
}

fn warn_legacy_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "rpc client sent no `proto` (legacy client, treated as protocol 0); \
             read-only calls are still accepted, every other call is refused: update the client"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, required_capability};

    const A: &str = "01J0000000000000000000000A";
    const B: &str = "01J0000000000000000000000B";

    fn unbound() -> BoundProject {
        BoundProject::default()
    }

    fn bound_a() -> BoundProject {
        BoundProject {
            project_id: Some(A.into()),
            via: BoundVia::Project,
        }
    }

    #[test]
    fn envelope_accepts_legacy_read_only_and_current() {
        assert!(envelope_refusal_with(&unbound(), "kernel.status", None, None).is_none());
        assert!(envelope_refusal_with(&unbound(), "x", Some(clawft_rpc::PROTO_VERSION), None).is_none());
    }

    #[test]
    fn envelope_refuses_legacy_mutating_request() {
        // `x.unlisted_mutation` is unknown to the capability table (defaults to
        // Read there) and must still be refused: the gate is an allowlist.
        for m in ["agent.spawn", "kernel.shutdown", "cron.add", "x.unlisted_mutation"] {
            let r = envelope_refusal_with(&unbound(), m, None, None).unwrap();
            assert_eq!(r.error_kind.as_deref(), Some("proto_mismatch"), "{m}");
        }
        // The discovery call stays open to a client with no proto.
        assert!(envelope_refusal_with(&unbound(), "kernel.handshake", None, None).is_none());
    }

    #[test]
    fn client_read_only_allowlist_is_read_capability() {
        for m in clawft_rpc::handshake::READ_ONLY_METHODS {
            assert_eq!(required_capability(m), Capability::Read, "{m}");
        }
    }

    #[test]
    fn envelope_refuses_unsupported_proto_without_exe() {
        let r = envelope_refusal_with(&unbound(), "x", Some(clawft_rpc::PROTO_VERSION + 1), None).unwrap();
        assert_eq!(r.error_kind.as_deref(), Some("proto_mismatch"));
        let d = r.data.unwrap();
        assert_eq!(d["daemon"]["proto"], clawft_rpc::PROTO_VERSION);
        assert!(d["daemon"].get("exe").is_none());
        assert!(envelope_refusal_with(&unbound(), "x", Some(0), None).is_some());
    }

    #[test]
    fn handshake_is_exempt_from_proto_and_mismatch_but_not_syntax() {
        let b = bound_a();
        assert!(envelope_refusal_with(&b, "kernel.handshake", Some(99), Some(B)).is_none());
        let r = envelope_refusal_with(&b, "kernel.handshake", Some(1), Some("../x")).unwrap();
        assert_eq!(r.error_kind.as_deref(), Some("invalid_project"));
    }

    #[test]
    fn envelope_refuses_malformed_project() {
        let r = envelope_refusal_with(&unbound(), "x", Some(1), Some("../x")).unwrap();
        assert_eq!(r.error_kind.as_deref(), Some("invalid_project"));
        assert!(envelope_refusal_with(&unbound(), "x", Some(1), Some(A)).is_none());
    }

    #[test]
    fn bound_daemon_refuses_other_projects_only() {
        let b = bound_a();
        let r = envelope_refusal_with(&b, "x", Some(1), Some(B)).unwrap();
        assert_eq!(r.error_kind.as_deref(), Some("project_mismatch"));
        assert!(envelope_refusal_with(&b, "x", Some(1), Some(A)).is_none());
        assert!(envelope_refusal_with(&b, "x", Some(1), None).is_none());
    }

    fn write_project(dir: &Path, id: &str) {
        std::fs::create_dir_all(dir.join(".weftos")).unwrap();
        std::fs::write(
            dir.join(".weftos/project.toml"),
            format!("schema = 1\nid = \"{id}\"\nname = \"p\"\ncreated = 2026-01-01T00:00:00Z\n"),
        )
        .unwrap();
    }

    fn write_manifest(mdir: &Path, id: &str, rt: &Path) {
        std::fs::create_dir_all(mdir).unwrap();
        std::fs::write(
            mdir.join(format!("{id}.toml")),
            format!(
                "schema = 1\nid = \"{id}\"\nname = \"p\"\nroot = \"/x\"\n\
                 created = 2026-01-01T00:00:00Z\nlast_seen = 2026-01-01T00:00:00Z\n\
                 [serve]\nruntime_dir = \"{}\"\n",
                rt.display()
            ),
        )
        .unwrap();
    }

    #[test]
    fn bound_from_project_toml() {
        let d = tempfile::tempdir().unwrap();
        write_project(d.path(), A);
        let paths = RuntimePaths::resolve_with(None, Some(d.path()), None);
        let b = compute_bound(&paths, None);
        assert_eq!((b.project_id.as_deref(), b.via), (Some(A), BoundVia::Project));
        let h = build_handshake("node".into(), &paths, None, &b);
        assert_eq!(h.project_id.as_deref(), Some(A));
        assert_eq!(h.bound_via, BoundVia::Project);
        assert_eq!((h.depth, h.parent), (0, None));
    }

    #[test]
    fn a_child_kernel_is_bound_to_its_project() {
        // The supervisor's readiness check and the verified-project path
        // both read this: a child that reported no project never got ready.
        let paths = RuntimePaths::child_at("/iso/run", A, "/work/p").unwrap();
        let b = compute_bound(&paths, None);
        assert_eq!((b.project_id.as_deref(), b.via), (Some(A), BoundVia::Project));
        let h = build_handshake("node".into(), &paths, None, &b);
        assert_eq!(h.project_id.as_deref(), Some(A));
    }

    #[test]
    fn bound_from_unique_manifest_runtime_dir() {
        let d = tempfile::tempdir().unwrap();
        let rt = d.path().join("rt");
        std::fs::create_dir_all(&rt).unwrap();
        let mdir = d.path().join("projects");
        write_manifest(&mdir, A, &rt);
        let paths = RuntimePaths::at(&rt);
        let b = compute_bound(&paths, Some(&mdir));
        assert_eq!((b.project_id.as_deref(), b.via), (Some(A), BoundVia::Manifest));
        // Two manifests claiming the same runtime dir: ambiguous, unbound.
        write_manifest(&mdir, B, &rt);
        assert_eq!(compute_bound(&paths, Some(&mdir)), BoundProject::default());
        // Different root: unbound.
        assert_eq!(
            compute_bound(&RuntimePaths::at(d.path().join("other")), Some(&mdir)),
            BoundProject::default()
        );
        // No manifests dir: unbound.
        assert_eq!(compute_bound(&paths, None), BoundProject::default());
    }
}
