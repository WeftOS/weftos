//! Tests for [`crate::scope_gate`].

use super::*;
use crate::capability::{CallerCapabilities, Capability, required_capability};
use crate::rpc_ext::{CallerCtx, KernelRef, authorize};
use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use clawft_types::config::{ChainConfig, Config, GovernanceConfig, KernelConfig};
use clawft_types::project::{ProjectManifest, write_manifest};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;

const A: &str = "01J0000000000000000000000A";
const B: &str = "01J0000000000000000000000B";
const C: &str = "01J0000000000000000000000C";

fn manifest(id: &str, state: ProjectState) -> ProjectManifest {
    let now = chrono::Utc::now();
    ProjectManifest {
        schema_version: 1,
        id: id.into(),
        name: "t".into(),
        root: std::env::temp_dir(),
        state,
        created: now,
        last_seen: now,
        project_toml: Default::default(),
        seed: None,
        legacy: None,
        serve: None,
        chain: None,
        binary: None,
        extra: Default::default(),
    }
}

async fn kernel_with(policy: OutsideProjectPolicy) -> KernelRef {
    let kcfg = KernelConfig {
        governance: GovernanceConfig { outside_project: Some(policy) },
        chain: Some(ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())),
        ..KernelConfig::default()
    };
    let k = Kernel::boot(Config::default(), kcfg, Arc::new(NativePlatform::new()))
        .await
        .expect("kernel boots");
    Arc::new(RwLock::new(k))
}

async fn roundtrip(kernel: &KernelRef, line: &str) -> Response {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (tx, _rx) = tokio::sync::watch::channel(false);
    tokio::spawn(crate::daemon::handle_connection(server, Arc::clone(kernel), tx));
    let (r, mut w) = tokio::io::split(client);
    w.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    let mut out = String::new();
    BufReader::new(r).read_line(&mut out).await.unwrap();
    serde_json::from_str(&out).unwrap()
}

// ---- outside-project definition ----

#[test]
fn inside_project_definition() {
    let d = tempfile::tempdir().unwrap();
    write_manifest(d.path(), &manifest(A, ProjectState::Active)).unwrap();
    write_manifest(d.path(), &manifest(B, ProjectState::Archived)).unwrap();
    let dir = Some(d.path());
    // Bound daemon: no claim or the same claim is inside; another is outside.
    assert!(inside_project(Some(A), None, dir));
    assert!(inside_project(Some(A), Some(A), dir));
    assert!(!inside_project(Some(A), Some(C), dir));
    // Even a registered project does not override a different binding.
    assert!(!inside_project(Some(C), Some(A), dir));
    // Unbound daemon: only a registered, active manifest verifies.
    assert!(!inside_project(None, None, dir));
    assert!(inside_project(None, Some(A), dir));
    assert!(!inside_project(None, Some(B), dir), "archived");
    assert!(!inside_project(None, Some(C), dir), "unregistered");
    assert!(!inside_project(None, Some("../x"), dir), "malformed");
    assert!(!inside_project(None, Some(A), None), "no registry");
    assert!(!inside_project(None, Some(A), Some(&d.path().join("missing"))));
}

#[test]
fn corrupt_manifest_does_not_verify() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join(format!("{A}.toml")), "not = [valid").unwrap();
    assert!(!inside_project(None, Some(A), Some(d.path())));
}

// ---- policies ----

#[test]
fn read_only_allows_only_the_list_outside() {
    for m in READ_ONLY_ALLOW {
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, false, || false).is_ok(), "{m}");
    }
    for m in ["agent.spawn", "cron.add", "kernel.shutdown", "no.such.method", ""] {
        let d = decide(OutsideProjectPolicy::ReadOnly, m, false, || false).unwrap_err();
        assert_eq!(d.kind, "project_required", "{m}");
        assert_eq!(d.message, SCOPE_MESSAGE);
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, false, || true).is_ok(), "{m} inside");
    }
}

#[test]
fn deny_all_keeps_only_liveness_and_project_lookup() {
    for m in DENY_ALL_ALLOW {
        assert!(decide(OutsideProjectPolicy::DenyAll, m, false, || false).is_ok(), "{m}");
    }
    for m in ["kernel.ps", "agent.list", "chain.status", "agent.spawn"] {
        assert!(decide(OutsideProjectPolicy::DenyAll, m, false, || false).is_err(), "{m}");
        assert!(decide(OutsideProjectPolicy::DenyAll, m, false, || true).is_ok(), "{m} inside");
    }
    assert!(DENY_ALL_ALLOW.iter().all(|m| READ_ONLY_ALLOW.contains(m)));
}

#[test]
fn allow_all_never_checks_the_project() {
    let r = decide(OutsideProjectPolicy::AllowAll, "agent.spawn", false, || panic!("not evaluated"));
    assert!(r.is_ok());
}

#[test]
fn allow_listed_methods_skip_registry_verification() {
    decide(OutsideProjectPolicy::ReadOnly, "kernel.status", false, || panic!("not evaluated")).unwrap();
}

// ---- population ----

/// Exact method strings and guard prefixes from the legacy `dispatch` match.
fn dispatch_arms() -> (Vec<String>, Vec<String>) {
    let src = include_str!("daemon.rs");
    let start = src.find("async fn dispatch(\n    method: String").expect("dispatch fn");
    let body = &src[start..];
    let body = &body[..body.find("\n}\n").expect("dispatch end")];
    let (mut exact, mut prefixes) = (Vec::new(), Vec::new());
    let mut pending: Option<String> = None;
    for line in body.lines() {
        if pending.is_none() {
            let Some(rest) = line.strip_prefix("        ") else { continue };
            if rest.starts_with("m if m.starts_with(\"") {
                prefixes.extend(literals(rest.split("=>").next().unwrap()));
                continue;
            }
            if !rest.starts_with('"') {
                continue;
            }
            pending = Some(String::new());
        }
        let buf = pending.as_mut().unwrap();
        buf.push_str(line);
        if line.contains("=>") {
            let pat = buf.split("=>").next().unwrap().to_owned();
            exact.extend(literals(&pat));
            pending = None;
        }
    }
    (exact, prefixes)
}

fn literals(s: &str) -> Vec<String> {
    s.split('"').skip(1).step_by(2).map(str::to_owned).collect()
}

/// Streaming intercepts matched in `dispatch_json_line` before `dispatch`.
const INTERCEPTS: &[&str] = &["ipc.subscribe_stream", "substrate.subscribe", "kernel.logs_stream", "chain.subscribe"];

/// Allow-listed methods whose handler is not a legacy arm (ext routes or
/// owned by other Phase 1 packages).
const NOT_LEGACY_ARMS: &[&str] = &["kernel.handshake", "project.list", "project.show", "auth.token.validate", "chain.subscribe", "fleet.snapshot", "route.list", "versions.list", "versions.tree", "versions.deps", "display.status"];

fn all_methods() -> Vec<String> {
    let (mut arms, _) = dispatch_arms();
    arms.extend(INTERCEPTS.iter().map(|s| (*s).to_owned()));
    arms.sort();
    arms.dedup();
    arms
}

#[test]
fn population_extraction_finds_the_dispatch_arms() {
    let (arms, prefixes) = dispatch_arms();
    assert!(arms.len() > 90, "parsed only {} arms", arms.len());
    for m in ["kernel.status", "agent.spawn", "cron.add", "cron.disable", "chain.local", "workspace.config.reset", "ping"] {
        assert!(arms.iter().any(|a| a == m), "missing {m}");
    }
    assert!(prefixes.iter().any(|p| p == "app.") && prefixes.iter().any(|p| p == "workload."));
}

/// Every dispatched method is either on the explicit allow-list or denied
/// outside a project under `read_only` and `deny_all`; `allow_all` passes
/// everything; nothing on the list is a mutating capability.
#[test]
fn population_every_dispatch_arm_is_allowed_or_denied_outside() {
    let methods = all_methods();
    let allowed: Vec<_> = methods
        .iter()
        .filter(|m| decide(OutsideProjectPolicy::ReadOnly, m, false, || false).is_ok())
        .cloned()
        .collect();
    let mut expect: Vec<_> = methods
        .iter()
        .filter(|m| READ_ONLY_ALLOW.contains(&m.as_str()))
        .cloned()
        .collect();
    expect.sort();
    assert_eq!(allowed, expect);
    for m in &methods {
        assert!(decide(OutsideProjectPolicy::AllowAll, m, false, || false).is_ok(), "{m}");
        let on_list = DENY_ALL_ALLOW.contains(&m.as_str());
        assert_eq!(decide(OutsideProjectPolicy::DenyAll, m, false, || false).is_ok(), on_list, "{m}");
        if required_capability(m) != Capability::Read {
            assert!(decide(OutsideProjectPolicy::ReadOnly, m, false, || false).is_err(), "{m} is not Read");
        }
    }
    // Sanity: the gate denies the bulk of the surface.
    assert!(methods.len() - allowed.len() > 70);
}

#[test]
fn population_allow_list_is_read_only_and_real() {
    let (arms, prefixes) = dispatch_arms();
    for m in READ_ONLY_ALLOW {
        assert_eq!(required_capability(m), Capability::Read, "{m} must be a Read method");
        assert!(
            arms.iter().any(|a| a == m) || NOT_LEGACY_ARMS.contains(m) || INTERCEPTS.contains(m),
            "{m} is on the allow-list but is not a dispatched method"
        );
        assert!(
            !prefixes.iter().any(|p| m.starts_with(p.as_str())),
            "{m} falls under a guard-prefix arm whose verbs are not individually reviewed"
        );
    }
    let dup: std::collections::HashSet<_> = READ_ONLY_ALLOW.iter().collect();
    assert_eq!(dup.len(), READ_ONLY_ALLOW.len());
}

/// Methods the `capability.rs` default would call anonymous `Read` but that
/// can disclose or change state must stay off the list; the list never
/// derives from capability. `chain.tail` (gateway facade) and the S2 reads
/// and streams used by first-party clients are the deliberate exceptions.
#[test]
fn allow_list_excludes_known_sensitive_reads() {
    // Deliberate exceptions: the gateway facade and first-party read-only
    // clients (S2 decision) need these outside a project.
    for m in ["chain.tail", "kernel.logs_stream", "substrate.subscribe", "substrate.read", "cluster.facts", "voice.trace"] {
        assert!(READ_ONLY_ALLOW.contains(&m), "{m}");
    }
    for m in ["chain.local", "chain.export", "agent.chat", "ipc.subscribe_stream"] {
        assert!(!READ_ONLY_ALLOW.contains(&m), "{m}");
    }
}

// ---- verified vs unverified claims, through the real gate ----

fn echo_req(method: &str, project: Option<&str>) -> String {
    let p = project.map(|p| format!(r#","project":"{p}""#)).unwrap_or_default();
    format!(r#"{{"method":"{method}","params":null,"auth":"write","proto":1,"id":"1"{p}}}"#)
}

/// Unbinds and clears the registry on drop.
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        crate::handshake_rpc::set_bound(Default::default());
        init(None, false);
    }
}

#[tokio::test]
async fn wire_scope_gate_denies_outside_and_verifies_claims() {
    let _serial = TEST_BOUND_LOCK.lock().await;
    let _reset = Reset;
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), &manifest(A, ProjectState::Active)).unwrap();
    init(Some(dir.path().to_path_buf()), false);
    crate::handshake_rpc::set_bound(Default::default());
    let kernel = kernel_with(OutsideProjectPolicy::ReadOnly).await;

    // No claim, unbound: a write-class probe is denied, a listed read passes.
    let r = roundtrip(&kernel, &echo_req("rpc_ext.test.echo", None)).await;
    assert_eq!(r.error_kind.as_deref(), Some("project_required"));
    assert_eq!(r.error.as_deref(), Some(SCOPE_MESSAGE));
    assert_eq!(r.id.as_deref(), Some("1"));
    assert!(roundtrip(&kernel, &echo_req("kernel.status", None)).await.ok);
    // Unregistered (unverified) claim: still outside.
    let r = roundtrip(&kernel, &echo_req("rpc_ext.test.echo", Some(C))).await;
    assert_eq!(r.error_kind.as_deref(), Some("project_required"));
    // Registered claim verifies against the manifest registry.
    let r = roundtrip(&kernel, &echo_req("rpc_ext.test.echo", Some(A))).await;
    assert!(r.ok, "{:?}", r.error);
    // A client that cannot be verified can't smuggle a mutating verb either.
    let r = roundtrip(&kernel, &echo_req("agent.spawn", Some(C))).await;
    assert_eq!(r.error_kind.as_deref(), Some("project_required"));
}

#[tokio::test]
async fn wire_bound_daemon_is_inside_and_mismatch_is_outside() {
    let _serial = TEST_BOUND_LOCK.lock().await;
    let _reset = Reset;
    let dir = tempfile::tempdir().unwrap();
    write_manifest(dir.path(), &manifest(B, ProjectState::Active)).unwrap();
    init(Some(dir.path().to_path_buf()), false);
    crate::handshake_rpc::set_bound(crate::handshake_rpc::BoundProject {
        project_id: Some(A.into()),
        via: clawft_rpc::handshake::BoundVia::Manifest,
    });
    let kernel = kernel_with(OutsideProjectPolicy::ReadOnly).await;
    // Bound and no claim: inside.
    assert!(roundtrip(&kernel, &echo_req("rpc_ext.test.echo", None)).await.ok);
    assert!(roundtrip(&kernel, &echo_req("rpc_ext.test.echo", Some(A))).await.ok);
    // Registered B is not this daemon's project: refused on the wire...
    let r = roundtrip(&kernel, &echo_req("rpc_ext.test.echo", Some(B))).await;
    assert_eq!(r.error_kind.as_deref(), Some("project_mismatch"));
    // ...and the gate does not rely on that: called directly it is outside.
    let (caps, null) = (CallerCapabilities::from_scopes(["write"]), Value::Null);
    let caller = CallerCtx {
        auth: Some("write".into()),
        project: Some(B.into()),
        ..CallerCtx::default()
    };
    let r = authorize(&caller, &caps, "rpc_ext.test.echo", &null, &kernel).await.unwrap_err();
    assert_eq!(r.error_kind.as_deref(), Some("project_required"));
}

#[tokio::test]
async fn wire_policies_deny_all_and_allow_all() {
    let _serial = TEST_BOUND_LOCK.lock().await;
    let _reset = Reset;
    init(Some(tempfile::tempdir().unwrap().keep()), false);
    crate::handshake_rpc::set_bound(Default::default());
    let deny = kernel_with(OutsideProjectPolicy::DenyAll).await;
    assert!(roundtrip(&deny, &echo_req("kernel.status", None)).await.ok);
    let r = roundtrip(&deny, &echo_req("kernel.ps", None)).await;
    assert_eq!(r.error_kind.as_deref(), Some("project_required"));
    let allow = kernel_with(OutsideProjectPolicy::AllowAll).await;
    assert!(roundtrip(&allow, &echo_req("rpc_ext.test.echo", None)).await.ok);
}

#[tokio::test]
async fn scope_denial_is_after_capability_check() {
    let _serial = TEST_BOUND_LOCK.lock().await;
    let _reset = Reset;
    crate::handshake_rpc::set_bound(Default::default());
    let kernel = kernel_with(OutsideProjectPolicy::ReadOnly).await;
    // An anonymous caller is told about capability, not scope, for an
    // Admin verb (no scope/existence signal before authorization).
    let line = r#"{"method":"kernel.shutdown","params":null,"proto":1}"#;
    let r = roundtrip(&kernel, line).await;
    assert!(r.error.unwrap().contains("permission denied"));
}

// ---- R4: voice may not mutate cron ----

#[tokio::test]
async fn voice_cannot_mutate_cron_but_can_list_and_spawn() {
    let kernel = kernel_with(OutsideProjectPolicy::AllowAll).await;
    let voice = CallerCtx::internal_voice();
    let caps = CallerCapabilities::from_scopes(voice.auth.clone().unwrap().split(',').map(str::to_owned));
    let null = Value::Null;
    for m in VOICE_DENIED {
        let e = authorize(&voice, &caps, m, &null, &kernel).await.unwrap_err();
        assert_eq!(e.error_kind.as_deref(), Some("voice_denied"), "{m}");
    }
    // A voice caller with a verified project claim is denied all the same.
    let mut scoped = CallerCtx::internal_voice();
    scoped.project = Some(A.into());
    assert!(authorize(&scoped, &caps, "cron.add", &null, &kernel).await.is_err());
    for m in ["cron.list", "agent.spawn", "kernel.status"] {
        assert!(authorize(&voice, &caps, m, &null, &kernel).await.is_ok(), "{m}");
    }
    // The same verbs still work for an authenticated external caller.
    let ext = CallerCtx::from_auth(Some("write".into()));
    let wcaps = CallerCapabilities::from_scopes(["write"]);
    for m in VOICE_DENIED {
        assert!(authorize(&ext, &wcaps, m, &null, &kernel).await.is_ok(), "{m}");
    }
}

#[test]
fn voice_deny_list_covers_every_cron_mutation() {
    let (arms, _) = dispatch_arms();
    let cron: Vec<_> = arms.iter().filter(|a| a.starts_with("cron.")).collect();
    assert!(cron.len() >= 5);
    for m in cron {
        let mutating = required_capability(m) != Capability::Read;
        assert_eq!(VOICE_DENIED.contains(&m.as_str()), mutating, "{m}");
    }
    for m in VOICE_DENIED {
        assert!(arms.iter().any(|a| a == m), "{m} is not a dispatched method");
    }
}

// ---- profile-keyed default, voice inside, user-level, routes ----

#[tokio::test]
async fn unset_policy_is_allow_all_off_the_user_profile_and_read_only_on_it() {
    let _serial = TEST_BOUND_LOCK.lock().await;
    let _reset = Reset;
    crate::handshake_rpc::set_bound(Default::default());
    let kcfg = KernelConfig {
        chain: Some(ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())),
        ..KernelConfig::default()
    };
    assert_eq!(kcfg.governance.outside_project, None);
    let k = Kernel::boot(Config::default(), kcfg, Arc::new(NativePlatform::new()))
        .await
        .unwrap();
    let kernel: KernelRef = Arc::new(RwLock::new(k));
    init(None, false);
    assert!(roundtrip(&kernel, &echo_req("rpc_ext.test.echo", None)).await.ok);
    init(None, true);
    let r = roundtrip(&kernel, &echo_req("rpc_ext.test.echo", None)).await;
    assert_eq!(r.error_kind.as_deref(), Some("project_required"));
}

#[tokio::test]
async fn voice_principal_is_inside_for_the_scope_gate() {
    let _serial = TEST_BOUND_LOCK.lock().await;
    let _reset = Reset;
    crate::handshake_rpc::set_bound(Default::default());
    let kernel = kernel_with(OutsideProjectPolicy::DenyAll).await;
    let voice = CallerCtx::internal_voice();
    let caps = CallerCapabilities::from_scopes(voice.auth.clone().unwrap().split(',').map(str::to_owned));
    let null = Value::Null;
    assert!(authorize(&voice, &caps, "agent.spawn", &null, &kernel).await.is_ok());
    // The cron deny-list still applies to it.
    let e = authorize(&voice, &caps, "cron.add", &null, &kernel).await.unwrap_err();
    assert_eq!(e.error_kind.as_deref(), Some("voice_denied"));
    // An external caller with the same scopes is outside and denied.
    let ext = CallerCtx::from_auth(voice.auth.clone());
    let e = authorize(&ext, &caps, "agent.spawn", &null, &kernel).await.unwrap_err();
    assert_eq!(e.error_kind.as_deref(), Some("project_required"));
}

#[test]
fn user_level_ops_need_admin_outside_a_project() {
    for m in USER_LEVEL_ALLOW {
        let r = |p, admin| decide(p, m, admin, || false);
        assert!(r(OutsideProjectPolicy::ReadOnly, true).is_ok(), "{m} admin");
        assert_eq!(r(OutsideProjectPolicy::ReadOnly, false).unwrap_err().kind, "project_required", "{m}");
        assert!(r(OutsideProjectPolicy::DenyAll, true).is_err(), "{m} deny_all");
        assert!(r(OutsideProjectPolicy::AllowAll, false).is_ok());
        assert!(!READ_ONLY_ALLOW.contains(m));
    }
    assert!(READ_ONLY_ALLOW.contains(&"auth.token.validate"));
}

/// Prefix routes whose verbs are individually listed above (everything
/// unlisted under them is denied outside a project by default).
const CLASSIFIED_PREFIXES: &[&str] = &["instance.nested.", "project.", "auth.token.", "shared.", "rpc_ext.test."];

/// Every registered ext route must be classified: allow-listed, user-level,
/// or a prefix whose verbs are classified. Fails when a package adds a route
/// without deciding what it means outside a project.
#[test]
fn population_every_ext_route_is_classified() {
    let routes = crate::rpc_ext::builtin_route_names();
    assert!(!routes.is_empty());
    for (name, _cap) in routes {
        let classified = if name.ends_with('.') {
            CLASSIFIED_PREFIXES.contains(&name)
        } else {
            READ_ONLY_ALLOW.contains(&name)
                || USER_LEVEL_ALLOW.contains(&name)
                || crate::licence_role_gate::is_licence_verb(name)
        };
        assert!(classified, "ext route {name:?} is not classified in scope_gate");
    }
}

#[test]
fn unlisted_verbs_under_classified_prefixes_are_denied() {
    for m in ["project.init", "project.archive", "auth.token.rotate", "rpc_ext.test.echo"] {
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, true, || false).is_err(), "{m}");
    }
}

/// mesh-local children are not Admin; they carry their project claim.
#[test]
fn mesh_local_routes_pass_for_a_child_inside_a_project_and_not_outside() {
    for m in ["mesh.challenge", "mesh.register", "mesh.heartbeat", "mesh.unregister"] {
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, false, || true).is_ok(), "{m} inside a project");
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, false, || false).is_err(), "{m} without a claim");
    }
}

/// ADR-106: the checkout verbs are treated alike outside a project. Checkout
/// and approve need Admin; status and the binding status are read-only, so
/// `weaver doctor` gets its `licence.*` findings (machine-level verbs, see
/// `licence_role_gate`).
#[test]
fn licence_checkout_verbs_are_allowed_alike_outside_a_project() {
    let ro = OutsideProjectPolicy::ReadOnly;
    for m in [
        "workload.cog.checkout",
        "workload.cog.checkout.approve",
        "workload.cog.checkout.release",
        "workload.cog.checkout.renew",
        "workload.node.reset-floor",
    ] {
        assert!(decide(ro, m, true, || false).is_ok(), "{m} for an admin");
        assert!(decide(ro, m, false, || false).is_err(), "{m} needs admin outside a project");
        assert_eq!(required_capability(m), Capability::Admin, "{m}");
    }
    for m in ["workload.cog.checkout.status", "workload.cog.checkout.list", "workload.node.binding"] {
        assert!(decide(ro, m, false, || false).is_ok(), "{m} is read-only and allowed");
        assert_eq!(required_capability(m), Capability::Read, "{m}");
    }
    // The doctor's two calls pass the read-only gate without a project (as
    // machine-level reads, not from the read-only list).
    for m in ["workload.node.binding", "workload.cog.checkout.status"] {
        assert!(!READ_ONLY_ALLOW.contains(&m) && decide(ro, m, false, || false).is_ok());
    }
}

/// ADR-106: the licence verbs are machine-level. They are on neither the
/// read-only nor the user-level list; outside a project they pass as the
/// capability table says (Read to anyone, Admin to Admin) under `read_only`,
/// never under `deny_all`, and `licence_role_gate` admits them only on the
/// machine's licence holder.
#[test]
fn licence_verbs_are_machine_level_not_user_level() {
    use crate::licence_role_gate::LICENCE_VERBS;
    for m in LICENCE_VERBS {
        assert!(!READ_ONLY_ALLOW.contains(m) && !USER_LEVEL_ALLOW.contains(m), "{m}");
        let read = required_capability(m) == Capability::Read;
        assert_eq!(decide(OutsideProjectPolicy::ReadOnly, m, false, || false).is_ok(), read, "{m} anonymous");
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, true, || false).is_ok(), "{m} admin");
        assert!(decide(OutsideProjectPolicy::DenyAll, m, true, || false).is_err(), "{m} deny_all");
    }
    // The read verbs are exactly the status ones.
    let reads: Vec<_> = LICENCE_VERBS.iter().filter(|m| required_capability(m) == Capability::Read).collect();
    assert_eq!(
        reads,
        vec![
            &"workload.node.binding",
            &"workload.cog.checkout.status",
            &"workload.cog.checkout.list",
            &"cog.check_run",
            &"cog.licence.status",
            &"cog.licence.claims",
            &"cog.licence.revoked",
        ]
    );
}

#[test]
fn owner_shutdown_is_admin_only_outside_a_project() {
    let method = "kernel.shutdown";
    assert_eq!(required_capability(method), Capability::Admin);
    assert!(decide(OutsideProjectPolicy::ReadOnly, method, true, || false).is_ok());
    assert!(decide(OutsideProjectPolicy::ReadOnly, method, false, || false).is_err());
    assert!(decide(OutsideProjectPolicy::DenyAll, method, true, || false).is_err());
}

/// ADR-117: driving a leaf display is a user-level act. Outside a project
/// an Admin caller (the owner's console host) may; anyone else may not.
#[test]
fn display_verbs_are_user_level_admin_outside_a_project() {
    for m in ["display.open", "display.ops", "display.poll", "display.close", "display.revoke"] {
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, true, || false).is_ok(), "{m} for Admin");
        assert!(decide(OutsideProjectPolicy::ReadOnly, m, false, || false).is_err(), "{m} for a non-Admin");
        assert!(decide(OutsideProjectPolicy::DenyAll, m, true, || false).is_err(), "{m} under deny_all");
    }
}

#[test]
fn display_status_is_a_read_outside_a_project() {
    assert!(decide(OutsideProjectPolicy::ReadOnly, "display.status", false, || false).is_ok());
}
