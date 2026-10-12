//! `clawft-weave` — shared library surface for the `weaver` CLI.
//!
//! The CLI binary lives in [`main.rs`](../../../src/main.rs). This lib
//! target re-exports the modules used by integration tests (notably
//! [`daemon::handle_connection`] for driving a kernel listener from a
//! temp socket). Keeping these modules public under `pub` rather than
//! `pub(crate)` is the minimum change required to make integration
//! tests link against them.

/// `app.*` daemon RPC handlers (mesh-placement-06).
pub mod app_rpc;
pub mod capability;
/// Tracing → pending-buffer bridge for ExoChain (WEFT-597).
pub mod chain_bridge;
pub mod chain_subscribe_rpc;
pub mod client;
pub mod commands;
pub mod control;
pub mod conv_postmortem;
// Local RPC daemon: Unix UDS + Windows named pipes (WEFT-559).
#[cfg(any(unix, windows))]
pub mod daemon;
/// Boot refusals (exit 78) and the SIGHUP re-exec plan.
#[cfg(any(unix, windows))]
pub mod boot_refusal;
/// Single-instance advisory lock on `<runtime>/kernel.lock` (ADR-103 P0b).
#[cfg(any(unix, windows))]
pub mod instance_lock;
#[cfg(any(unix, windows))]
pub mod llm_service;
/// `kernel.handshake` and the request-envelope gate (ADR-103 D14).
#[cfg(any(unix, windows))]
pub mod handshake_rpc;
/// `project.*` RPCs over the per-user manifest store (ADR-103 Phase 1).
#[cfg(any(unix, windows))]
pub mod project_cert_rpc;
/// `project.anchor.submit`: a project's signed chain-head statement (ADR-103 A7).
#[cfg(any(unix, windows))]
pub mod anchor_rpc;
pub mod project_rpc;
/// The per-user daemon profile (`weaver kernel start --profile user`).
#[cfg(any(unix, windows))]
pub mod user_daemon;
/// The user key (`~/.weftos/user.key`) and its migration from `chain.key` (ADR-103 D-5).
pub mod user_key;
/// Classifying a unix-socket peer: owner, other uid or a supervised child's process group (ADR-103 A14).
pub mod child_peer;
/// `weaver migrate user-key --rotate`: rotation with a dual-signed handover (ADR-103 A13).
#[cfg(any(unix, windows))]
pub mod user_key_rotate;
/// Mesh mode as reported in the handshake (ADR-103 P3-U).
pub mod mesh_state;
/// `weaver doctor` mesh-service checks (P3-H).
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_doctor;
/// Installer tiers: service/user/project skew and the printed service update lines (P3-H).
pub mod install_tiers;
/// Chain events the mesh link records (journal anchors, service binding), queued until appended.
pub mod mesh_local_chain;
/// Client side of the machine mesh service: link, registration, delivery, verdicts (P3-U).
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_local_glue;
/// Inbound delivery into the A2A router and outbound remote forwarding (P3-U).
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_local_sink;
/// Boot glue: mesh mode and node identity before the kernel, the link after it.
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_boot;
/// Cog mesh wiring: artifact tunnel and checkout handler over stamped deliveries (ADR-106 1c).
#[cfg(all(unix, feature = "mesh", feature = "ecc", feature = "exochain"))]
pub mod cog_swarm;
/// `verdict.request` answered by the governance gate (P3-U).
#[cfg(all(unix, feature = "mesh"))]
pub mod mesh_local_verdict;
/// Daemon RPC extension seam: method-prefix routes + pre-dispatch gates (ADR-103 D0).
#[cfg(any(unix, windows))]
pub mod rpc_ext;
/// `mesh.*` (mesh-local/1): the user daemon's child registry (ADR-103 Phase 2 H).
#[cfg(unix)]
pub mod mesh_local_registry;
/// `mesh.challenge|register|heartbeat|unregister` handlers (ADR-103 Phase 2 H).
#[cfg(unix)]
pub mod mesh_local_rpc;
/// Child bootstrap: spawn handshake, key, registration, certificate (ADR-103 Phase 2 H).
#[cfg(unix)]
pub mod project_boot;
/// The child's wire to the user daemon and the real anchor transport (ADR-103 Phase 2 H).
#[cfg(unix)]
pub mod project_boot_link;
/// The child's running half: genesis, heartbeat, anchors, shutdown (ADR-103 Phase 2 H).
#[cfg(unix)]
pub mod project_boot_run;
/// Seams `daemon::run` calls for the per-project kernel profile (ADR-103 A6).
#[cfg(any(unix, windows))]
pub mod project_hooks;
/// Read a running process's real environment (supervisor tests, ADR-103 Phase 2 F).
#[cfg(any(unix, windows))]
pub mod env_probe;
/// Parent-side client for the shared services of a `project`-profile kernel (ADR-103 Phase 2 F).
#[cfg(any(unix, windows))]
pub mod parent_link;
/// Remote embedder and LLM backend over the parent link (ADR-103 Phase 2 F).
#[cfg(any(unix, windows))]
pub mod parent_services;
/// The `project`-profile service adjustments (ADR-103 Phase 2 F).
#[cfg(any(unix, windows))]
pub mod project_profile;
/// Per-project rate limit and token budget for `shared.*` (ADR-103 Phase 2 F).
#[cfg(any(unix, windows))]
pub mod shared_meter;
/// Process state behind `shared.*`: meter, cached limits, permits, model allow-list (ADR-103 Phase 2 F).
#[cfg(any(unix, windows))]
pub mod shared_state;
/// `shared.*` RPCs: the user daemon's embedding and LLM services for its projects (ADR-103 Phase 2 F).
#[cfg(any(unix, windows))]
pub mod shared_rpc;
/// `VerifiedProject`: a project id with cryptographic provenance (ADR-103 A6).
#[cfg(any(unix, windows))]
pub mod verified_project;
/// User-signed forward header sign and verify (ADR-103 A6, package I).
#[cfg(any(unix, windows))]
pub mod project_forward;
/// Establishes the caller's `VerifiedProject` per request (package I).
#[cfg(any(unix, windows))]
pub mod caller_principal;
/// ADR-106: the licence verbs are served by the machine's licence holder only.
#[cfg(any(unix, windows))]
pub mod licence_role_gate;
/// D12 scope gate: outside-project policy and the voice deny-list (ADR-103).
#[cfg(any(unix, windows))]
pub mod scope_gate;
/// `governance.parent.push|update` and `governance.reload` (ADR-103 D8).
#[cfg(any(unix, windows))]
pub mod governance_push;
/// The project supervisor: per-project child kernels under the user daemon
/// (ADR-103 A6, Phase 2 package G).
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
pub mod project_supervisor;
/// The method allow-list of a project token (ADR-103 A6, package G).
#[cfg(any(unix, windows))]
pub mod project_token_scope;
/// Open-stream counter (idle-stop input of a project kernel).
#[cfg(any(unix, windows))]
pub mod open_streams;
/// `weaver project migrate-kernel` logic (ADR-103 A6, package G).
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
pub mod project_migrate;
/// `project.start|stop|restart|status|ensure_running` (ADR-103 A6, package G).
#[cfg(all(unix, feature = "exochain", feature = "placement"))]
pub mod project_lifecycle_rpc;
/// The lifecycle routes' answer in a build without the supervisor. Always
/// compiled so default-feature tests exercise it (the self dev-dependency
/// turns the default features back on, so `--no-default-features` test runs
/// are not possible); it is [`project_lifecycle_rpc`] where the supervisor is
/// not built.
pub mod project_lifecycle_stub;
/// Without the supervisor (non-unix, or no `exochain`/`placement`) the
/// lifecycle routes still exist and answer `not_user_daemon`.
#[cfg(not(all(unix, feature = "exochain", feature = "placement")))]
pub use project_lifecycle_stub as project_lifecycle_rpc;
/// `auth.token.*` RPC handlers and the per-kernel token authority (ADR-102 D3).
#[cfg(any(unix, windows))]
pub mod token_rpc;
/// TCP-relay request sanitiser: strips self-asserted scope strings (ADR-102 D3).
#[cfg(unix)]
pub mod relay_auth;
/// Live MCP registry RPC handlers (WEFT-494 / ADR-070). Available on all
/// platforms so unit tests can exercise add/list/remove without UDS.
pub mod mcp_rpc;
pub mod node_identity;
/// Mic source node discovery for whisper / classify (ADR-103 D11 follow-up).
pub mod mic_source;
/// Node facts probe, signing, cache and `cluster.facts` (mesh-placement-03).
#[cfg(any(feature = "mesh", feature = "exochain"))]
#[cfg(all(feature = "placement", unix))]
pub mod infer_cfg;
#[cfg(all(feature = "placement", unix))]
pub mod infer_expose;
#[cfg(all(feature = "placement", unix))]
pub mod infer_managed;
#[cfg(all(feature = "placement", unix))]
pub mod infer_rpc;
#[cfg(all(feature = "placement", unix))]
pub mod infer_wire;
#[cfg(all(test, feature = "placement", unix))]
pub(crate) mod infer_wire_tests;
#[cfg(all(test, feature = "placement", unix))]
mod infer_managed_tests;
#[cfg(all(test, feature = "placement", unix))]
mod infer_cfg_tests;
#[cfg(all(test, feature = "placement", unix))]
mod infer_hardening_tests;
pub mod node_facts_rpc;
/// Operator location labels for the fleet manager (site and room per node).
pub mod dashboard_actions;
pub mod dashboard_cfg;
pub mod dashboard_budget;
/// Package digest enforcement before an installed agent starts (ADR-121 O2).
pub mod instance_digest;
/// The node's instance registry, chain records and heartbeat summary (ADR-121 O2).
pub mod instance_registry;
/// Pull-style runners feeding the instance registry (ADR-121 O2).
pub mod instance_sources;
pub mod dashboard_report;
pub mod dashboard_rpc;
pub mod dashboard_token;
pub mod dashboard_workspaces;
// ADR-116 R1: the tailnet router inside the user daemon.
pub mod router_cfg;
pub mod router_index;
pub mod router_proxy;
pub mod router_routes;
pub mod router_rpc;
pub mod router_serve;
pub mod router_sources;
pub mod router_state;
// ADR-116 R2/R3: dashboard overlays, the `route` action and `report.routes`.
pub mod dashboard_routes;
pub mod dashboard_services;
pub mod router_action;
pub mod router_overlay;
pub mod project_git;
pub mod project_install;
pub mod project_install_git;
pub mod project_install_handlers;
pub mod project_install_layout;
#[cfg(all(test, unix))]
mod project_install_test_support;
/// Pairing two nodes for project work from the dashboard (ADR-108 P2b).
pub mod mesh_pair;
/// What this node paired with, by project (resolves weftos:// project names).
pub mod mesh_pairings;
/// `weftos://` names (ADR-114).
pub mod weftos_uri;
/// Pending pair requests, reported in the heartbeat (ADR-108 P2b).
pub mod mesh_pair_requests;
/// `project-fetch.json`: per-peer fetch grants (ADR-108 P2b writes, P3b enforces).
pub mod project_fetch_grants;
/// The daemon's pair source, `mesh.pair.*` RPCs and handler wiring (ADR-108 P2b).
#[cfg(all(feature = "placement", unix))]
pub mod mesh_pair_rpc;
#[cfg(test)]
pub(crate) mod dashboard_test_support;
pub mod fleet_labels;
/// `fleet.snapshot` and `fleet.location.set` (fleet manager P1).
pub mod fleet_rpc;
#[cfg(feature = "ecc")]
pub mod spatial_rpc;
pub mod protocol;
pub mod service_units;
pub mod service_units_system;
/// Governance-gate helper for daemon RPC families (mesh-placement-06).
pub mod rpc_gate;
// WEFT-720 residual: `spatial_rpc` dropped when BvhStore/CLI helpers
// diverged; reattach via SpatialService before re-enabling spatial_cli_e2e.
#[cfg(feature = "rvf-rpc")]
pub mod rvf_codec;
#[cfg(feature = "rvf-rpc")]
pub mod rvf_rpc;
#[cfg(feature = "exochain")]
pub mod turn_ledger;
pub mod voice_loop;
pub mod voice_router;
pub mod voice_trace;
/// Node-local workload catalog (ADR-099, mesh-placement-06).
pub mod workload_registry;
/// `workload.*` daemon RPC family (ADR-099, mesh-placement-06).
#[cfg(feature = "exochain")]
pub mod workload_rpc;
/// Placement control plane RPCs (ADR-099, mesh-placement-12).
#[cfg(all(feature = "placement", unix))]
pub mod workload_place_rpc;
/// Operator policy files for placement (mesh-placement-12).
#[cfg(all(feature = "placement", unix))]
pub mod workload_place_policy;
/// The daemon's gate for `workload.*` actions: default deny plus permits.
#[cfg(all(feature = "placement", unix))]
pub mod workload_gate;
/// `workload.revoke`: operator revocation with forced unload.
#[cfg(all(feature = "placement", unix))]
pub mod workload_revoke_rpc;
/// Seed licence path at daemon boot: mesh id, checkout policy, binder (ADR-106).
#[cfg(all(feature = "placement", unix))]
pub mod licence_boot;
// ADR-106 phase 3: the placement signer (node key, or the service-mode control key).
#[cfg(all(feature = "placement", unix))]
pub mod placement_boot;
/// `workload.node.bind | unbind | binding` (ADR-106 phase 1d).
#[cfg(all(feature = "placement", unix))]
pub mod licence_rpc;
/// `weaver doctor` findings for the Seed licence path (ADR-106).
#[cfg(all(feature = "placement", unix))]
pub mod licence_doctor;
/// The steward relay from `licence-link.json` (ADR-106 phase 3).
#[cfg(all(feature = "placement", unix))]
pub mod licence_steward;
/// `workload.cog.checkout | approve | status` (ADR-106 phase 3).
#[cfg(all(feature = "placement", unix))]
pub mod licence_checkout_rpc;
/// `workload.cog.checkout.release | renew | list` (ADR-106 phase 3).
#[cfg(all(feature = "placement", unix))]
pub mod licence_checkout_verbs;
/// This node's cog ingest bridge and store owner (mesh-placement-10).
#[cfg(all(feature = "placement", unix))]
pub mod cog_ingest_serve;
/// This node's `workload-host`, served to other controllers (mesh-placement-12).
#[cfg(all(feature = "placement", unix))]
pub mod workload_host_serve;
/// ADR-108 P3b: the fetch gate (peer tier plus grant) and the fetch-peer controller policy.
#[cfg(all(feature = "placement", unix))]
pub mod project_fetch_policy;
/// ADR-108 P3b: the primary's repositories, refs and bundles.
#[cfg(all(feature = "placement", unix))]
pub mod project_fetch_repos;
/// ADR-108 P3b: non-git content (archive list, tar build and checked unpack).
#[cfg(all(feature = "placement", unix))]
pub mod project_fetch_tar;
/// ADR-108 P3b: `project.fetch` served on the primary's `workload-host`.
#[cfg(all(feature = "placement", unix))]
pub mod project_fetch_serve;
/// ADR-108 P3b: the member's channel, URL and chunked download.
#[cfg(all(feature = "placement", unix))]
pub mod project_fetch_client;
/// ADR-114: which weftos:// authorities name this node's own mesh.
#[cfg(all(feature = "placement", unix))]
pub mod mesh_names;
/// ADR-108 P3b: the `mesh` project fetcher.
#[cfg(all(feature = "placement", unix))]
pub mod project_fetch_mesh;
/// ADR-108 P3b: local RPC `project.fetch` for `git-remote-weftos`.
#[cfg(all(feature = "placement", unix))]
pub mod project_fetch_rpc;

#[cfg(unix)]
pub mod parent_liveness;

#[cfg(all(unix, feature = "exochain"))]
pub mod nested_boot;
#[cfg(all(unix, feature = "exochain"))]
pub mod nested_supervisor;
#[cfg(all(unix, feature = "exochain"))]
pub mod nested_rpc;
