//! Daemon RPC extension seam (ADR-103 Phase 1, package D0).
//!
//! `daemon.rs` has one big `match` in `dispatch` and one choke point in
//! `dispatch_json_line`. Every package that adds RPC surface would collide
//! there, so new methods register here instead:
//!
//! * **Routes** ([`ExtRoute`]): a method prefix (`"project."`,
//!   `"auth.token."`) or an exact name (`"kernel.handshake"`) mapped to an
//!   async handler **and an explicit required [`Capability`]**. The
//!   capability table in `capability.rs` defaults unlisted methods to
//!   `Read` (anonymous-callable), so a route must state what it needs;
//!   [`authorize`] enforces it. A `None` from [`dispatch_ext`] means "not
//!   mine" and the legacy `match` runs.
//! * **Gates** ([`GateFn`]): hooks that see every authorized-so-far request
//!   and may deny it with a structured error (`error_kind` set). They run
//!   *after* capability resolution and receive the resolved capabilities,
//!   so a gate never sees a request from a caller who failed the
//!   capability check (no pre-auth existence leaks). Package G's D12 scope
//!   gate lands here.
//!
//! Every entry path (JSON lines, RVF frames, the in-process voice
//! consumer) builds a [`CallerCtx`] and goes through [`authorize`] in
//! `daemon.rs`, so there is one place where authorization happens.
//!
//! Ext routes cannot override the streaming intercepts in
//! `dispatch_json_line` (`ipc.subscribe_stream`, `substrate.subscribe`,
//! `kernel.logs_stream`): those take over the connection and are matched
//! before routes are consulted. They are still authorized and gated.
//!
//! A route whose prefix covers an intercepted method would change that
//! method's required capability while its handler never runs, so routes
//! must not cover them (a test asserts this for `ROUTES`).
//!
//! There is no transport marker: the TCP relay forwards into the unix
//! socket, so a relayed caller looks like a local one and no gate may rely
//! on the difference. Instead the relay (`relay_auth`) strips self-asserted
//! literal scope strings from relayed requests, and the daemon honours
//! literal scopes only from a unix peer with its own uid
//! ([`CallerCtx::peer_untrusted`]). Token secrets work on any path.
//!
//! Registration is explicit and stateless: the `ROUTES` and `GATES` const
//! tables below, one line per package. There is no global mutable
//! registry; handlers reach shared state through [`ExtCtx::kernel`].
//! Tests build their own [`ExtRegistry`] and use the `*_with` entry points.

use std::borrow::Cow;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use clawft_kernel::boot::Kernel;
use clawft_platform::NativePlatform;
use clawft_rpc::Response;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::capability::{CallerCapabilities, Capability, required_capability};

/// Shared kernel handle, same shape `daemon.rs` passes around.
pub type KernelRef = Arc<RwLock<Kernel<NativePlatform>>>;

/// A project id the client CLAIMED in `Request.project`.
///
/// UNVERIFIED: the daemon only checks that it is a well-formed ULID and
/// that it equals the daemon's bound project (when it has one). Holding a
/// `ClaimedProject` proves nothing about membership; package G must verify
/// it against the project registry before any scope decision relies on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedProject(String);

impl ClaimedProject {
    /// The claimed id, unverified.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for ClaimedProject {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ClaimedProject {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

pub use crate::verified_project::VerifiedProject;

/// The kind of principal behind a request, set by the entry path (never by
/// the client). Gates use it for per-principal deny-lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Principal {
    /// A socket / RVF caller.
    #[default]
    External,
    /// The daemon's in-process voice consumer ([`CallerCtx::internal_voice`]).
    InternalVoice,
}

/// Which local socket admitted this request. The child endpoint enforces a
/// method ceiling before bearer-token capability resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RpcEndpoint {
    #[default]
    Owner,
    Child,
}

/// Who is calling, as established by the entry path before dispatch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallerCtx {
    pub endpoint: RpcEndpoint,
    /// Set by the entry path; see [`Principal`].
    pub principal: Principal,
    /// Bearer / scope token from the request envelope, if any.
    pub auth: Option<String>,
    /// Project the client claims the request is scoped to
    /// (`Request.project`). Unverified; see [`ClaimedProject`].
    pub project: Option<ClaimedProject>,
    /// The raw forward header of the request, unverified. Only
    /// [`crate::caller_principal::establish`] reads it.
    pub forward: Option<clawft_rpc::ForwardHeader>,
    /// Project proven by a token scope, a verified forward header or this
    /// kernel's own binding, set by the entry path (never from
    /// `Request.project`). `None` means "claim only". See
    /// [`crate::caller_principal`] for the honest limit.
    pub verified_project: Option<VerifiedProject>,
    /// The connection's peer is NOT the daemon's own uid (unix socket
    /// peer credentials). Such a caller cannot use literal scope strings
    /// (`"admin"`, ...) as auth; only a token secret counts. `false` for
    /// in-process callers and for the default.
    pub peer_untrusted: bool,
    /// The peer is the daemon's uid but inside a supervised project kernel's
    /// process group ([`crate::child_peer`]). Implies `peer_untrusted`; its
    /// literal scopes are ignored (anonymous), not refused, because a child
    /// legitimately calls read-level parent methods with the client's
    /// implicit `"admin"`. Only its project token is honoured.
    pub peer_child: bool,
}

impl CallerCtx {
    /// A caller presenting `auth` (or none).
    pub fn from_auth(auth: Option<String>) -> Self {
        Self {
            endpoint: RpcEndpoint::Owner,
            principal: Principal::External,
            auth,
            project: None,
            forward: None,
            verified_project: None,
            peer_untrusted: false,
            peer_child: false,
        }
    }

    /// Mark the connection peer as not the daemon's uid.
    pub fn with_peer_untrusted(mut self, untrusted: bool) -> Self {
        self.peer_untrusted = untrusted;
        self
    }

    /// Set the peer classification: anything but the owner is untrusted, and a
    /// supervised child is additionally marked `peer_child`.
    pub fn with_peer(mut self, peer: crate::child_peer::PeerClass) -> Self {
        self.peer_untrusted = !peer.is_owner();
        self.peer_child = peer == crate::child_peer::PeerClass::Child;
        self
    }

    pub fn with_endpoint(mut self, endpoint: RpcEndpoint) -> Self {
        self.endpoint = endpoint;
        if endpoint == RpcEndpoint::Child {
            self.peer_untrusted = true;
            self.peer_child = true;
        }
        self
    }

    /// Caller context for a wire request: its `auth` and `project`.
    pub fn from_request(req: &clawft_rpc::Request) -> Self {
        Self {
            endpoint: RpcEndpoint::Owner,
            principal: Principal::External,
            auth: req.auth.clone(),
            project: req.project.clone().map(ClaimedProject::from),
            forward: req.forward.clone(),
            verified_project: None,
            peer_untrusted: false,
            peer_child: false,
        }
    }

    /// The daemon's in-process voice consumer.
    ///
    /// Decision: `read,chat,write`, never `admin`. Voice Level 2 is an
    /// operator opt-in to route arbitrary spoken verbs, so mutating verbs
    /// must work, but daemon-lifecycle and trust-root verbs
    /// (`kernel.shutdown`, `kernel.kill-process`, `cluster.*`,
    /// `workload.revoke`, `chain.checkpoint`) stay out of reach of speech.
    /// Before this the voice path dispatched with no check at all.
    ///
    /// The scope gate additionally denies this principal the cron
    /// mutations (`cron.*` writes), see `scope_gate::VOICE_DENIED`.
    pub fn internal_voice() -> Self {
        Self {
            principal: Principal::InternalVoice,
            ..Self::from_auth(Some("read,chat,write".to_owned()))
        }
    }
}

/// Per-request context handed to route handlers.
#[derive(Clone)]
pub struct ExtCtx {
    pub kernel: KernelRef,
    pub auth: Option<String>,
    /// Unverified client claim; see [`ClaimedProject`].
    pub project: Option<ClaimedProject>,
    /// Verified project of the caller, if any; see [`CallerCtx::verified_project`].
    pub verified_project: Option<VerifiedProject>,
    /// Capabilities already resolved for this caller.
    pub caps: CallerCapabilities,
}

/// One dispatched call: the method that matched plus its parameters.
pub struct ExtCall {
    pub method: String,
    pub params: Value,
    pub ctx: ExtCtx,
}

pub type ExtFuture = Pin<Box<dyn Future<Output = Response> + Send>>;
pub type ExtHandler = fn(ExtCall) -> ExtFuture;

/// A method-prefix (or exact-name) handler registration.
#[derive(Clone, Copy)]
pub struct ExtRoute {
    /// A prefix ending in `.` matches every method under it; any other
    /// string matches that exact method only.
    pub prefix: &'static str,
    /// Capability a caller must hold. Explicit on purpose: unlisted
    /// methods default to anonymous-callable `Read` in `capability.rs`.
    pub capability: Capability,
    pub handler: ExtHandler,
}

impl ExtRoute {
    fn matches(&self, method: &str) -> bool {
        if self.prefix.ends_with('.') {
            method.starts_with(self.prefix)
        } else {
            method == self.prefix
        }
    }
}

/// What a gate sees. Borrowed so running gates never clones params.
pub struct GateRequest<'a> {
    /// Who is calling (set by the entry path, not the client).
    pub principal: Principal,
    pub method: &'a str,
    pub params: &'a Value,
    pub auth: Option<&'a str>,
    /// Unverified client claim (see [`ClaimedProject`]).
    pub project: Option<&'a str>,
    /// Verified project of the caller, if any (see [`CallerCtx::verified_project`]).
    pub verified_project: Option<&'a VerifiedProject>,
    /// Capabilities resolved for the caller (the capability check has
    /// already passed for `method`).
    pub caps: &'a CallerCapabilities,
    pub kernel: &'a KernelRef,
}

/// A gate's refusal, surfaced as `Response::error_with_kind(kind, message)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// snake_case discriminator for clients (e.g. `"scope_denied"`).
    pub kind: String,
    pub message: String,
}

impl Denial {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

pub type GateFuture<'a> = Pin<Box<dyn Future<Output = Result<(), Denial>> + Send + 'a>>;
pub type GateFn = for<'a> fn(&'a GateRequest<'a>) -> GateFuture<'a>;

/// Registered routes, in match order (first match wins).
///
/// Each later package adds one line here plus its own module, e.g.
/// `ExtRoute { prefix: "project.", capability: Capability::Read,
/// handler: crate::project_rpc::handle }`.
#[cfg(not(test))]
const ROUTES: &[ExtRoute] = &[
    #[cfg(all(unix, feature = "exochain"))]
    ExtRoute {
        prefix: "instance.nested.",
        capability: Capability::Admin,
        handler: crate::nested_rpc::handle,
    },
    ExtRoute {
        prefix: "kernel.handshake",
        capability: Capability::Read,
        handler: crate::handshake_rpc::handle,
    },
    ExtRoute {
        prefix: "project.list",
        capability: Capability::Read,
        handler: crate::project_rpc::handle_list,
    },
    ExtRoute {
        prefix: "project.show",
        capability: Capability::Read,
        handler: crate::project_rpc::handle_show,
    },
    ExtRoute {
        prefix: "project.register",
        capability: Capability::Admin,
        handler: crate::project_rpc::handle_register,
    },
    ExtRoute {
        prefix: "project.nested.register",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.nested.start",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.nested.stop",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.cert.show",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.cert.challenge",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.identity.repair",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.rekey",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.revoke",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    // Read: authenticated by the project key through the statement
    // signature, not by a token (a project kernel holds none).
    ExtRoute {
        prefix: "project.anchor.submit",
        capability: Capability::Read,
        handler: crate::anchor_rpc::handle,
    },
    ExtRoute {
        prefix: "project.anchor.restore",
        capability: Capability::Admin,
        handler: crate::anchor_rpc::handle_restore,
    },
    ExtRoute {
        prefix: "project.anchor.reset",
        capability: Capability::Admin,
        handler: crate::anchor_rpc::handle_reset,
    },
    // Per-project child kernels (ADR-103 A6, package G). Lifecycle is Admin;
    // `project.token.refresh` is Write so a project token can renew itself.
    ExtRoute {
        prefix: "project.start",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.ensure_running",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.stop",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.stop_all",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.restart",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.status",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.token.refresh",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "auth.token.issue",
        capability: Capability::Admin,
        handler: crate::token_rpc::handle,
    },
    ExtRoute {
        prefix: "auth.token.revoke",
        capability: Capability::Admin,
        handler: crate::token_rpc::handle,
    },
    ExtRoute {
        prefix: "auth.token.list",
        capability: Capability::Admin,
        handler: crate::token_rpc::handle,
    },
    // Read: the gateway and any anonymous caller may ask "is this secret
    // valid?"; a 256-bit secret cannot be searched for.
    ExtRoute {
        prefix: "auth.token.validate",
        capability: Capability::Read,
        handler: crate::token_rpc::handle,
    },
    // Fleet manager P1: the snapshot is read-only and contacts no peer; a
    // location label is an operator decision recorded on the chain.
    ExtRoute {
        prefix: "fleet.snapshot",
        capability: Capability::Read,
        handler: crate::fleet_rpc::handle,
    },
    ExtRoute {
        prefix: "fleet.location.set",
        capability: Capability::Admin,
        handler: crate::fleet_rpc::handle,
    },
    // ADR-121 version index: read-only queries over the local version files.
    ExtRoute {
        prefix: "versions.list",
        capability: Capability::Read,
        handler: crate::versions_rpc::handle,
    },
    ExtRoute {
        prefix: "versions.tree",
        capability: Capability::Read,
        handler: crate::versions_rpc::handle,
    },
    ExtRoute {
        prefix: "versions.deps",
        capability: Capability::Read,
        handler: crate::versions_rpc::handle,
    },
    // Dashboard reporter: status is read-only; rotating the node token is Admin
    // (with `node`, it is sent to that peer over the signed mesh wire).
    ExtRoute {
        prefix: "dashboard.status",
        capability: Capability::Read,
        handler: crate::dashboard_rpc::handle,
    },
    ExtRoute {
        prefix: "dashboard.actions",
        capability: Capability::Read,
        handler: crate::dashboard_rpc::handle,
    },
    ExtRoute {
        prefix: "dashboard.token.rotate",
        capability: Capability::Admin,
        handler: crate::dashboard_rpc::handle,
    },
    // ADR-116: the tailnet router. Both are local reads of the owner's own
    // compose/ports.yaml files (the poller re-reads them anyway).
    ExtRoute {
        prefix: "route.list",
        capability: Capability::Read,
        handler: crate::router_rpc::handle,
    },
    ExtRoute {
        prefix: "route.reload",
        capability: Capability::Read,
        handler: crate::router_rpc::handle,
    },
    // ADR-108 P2b: pending pair requests on this node (request and cancel
    // are trust acts; list shows ids and node ids only).
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "mesh.pair.request",
        capability: Capability::Admin,
        handler: crate::mesh_pair_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "mesh.pair.list",
        capability: Capability::Read,
        handler: crate::mesh_pair_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "mesh.pair.cancel",
        capability: Capability::Admin,
        handler: crate::mesh_pair_rpc::handle,
    },
    // One prefix for shared.embed / shared.llm.chat / shared.llm.models: the
    // handler resolves the project itself (token scope or Admin) and refuses
    // anonymous callers; `Write` is the floor.
    // ADR-117 D1/D2: an app drives a leaf display through the daemon. Exact
    // names, not a prefix: each verb is classified in `scope_gate`.
    ExtRoute {
        prefix: "display.open",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.ops",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.poll",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.close",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.revoke",
        capability: Capability::Admin,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.status",
        capability: Capability::Read,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "shared.",
        capability: Capability::Write,
        handler: crate::shared_rpc::handle,
    },
    // Governance overlay (ADR-103 D8): push runs on the user daemon, update
    // and reload on a project kernel; all Admin.
    ExtRoute {
        prefix: "governance.parent.push",
        capability: Capability::Admin,
        handler: crate::governance_push::handle_push,
    },
    ExtRoute {
        prefix: "governance.parent.update",
        capability: Capability::Admin,
        handler: crate::governance_push::handle_update,
    },
    ExtRoute {
        prefix: "governance.reload",
        capability: Capability::Admin,
        handler: crate::governance_push::handle_reload,
    },
    // ADR-106 phase 3: Admin (a checkout spends licence budget); routed here
    // so the relay rate-limits and gates the caller's own principal.
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "workload.cog.checkout",
        capability: Capability::Admin,
        handler: crate::licence_checkout_rpc::handle_ext,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "workload.cog.checkout.release",
        capability: Capability::Admin,
        handler: crate::licence_checkout_rpc::handle_ext,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "workload.cog.checkout.renew",
        capability: Capability::Admin,
        handler: crate::licence_checkout_rpc::handle_ext,
    },
    // ADR-108 P3b: `git-remote-weftos` asks this daemon to fetch from a
    // project's primary over the signed mesh wire. Contacting a peer is Admin.
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "project.fetch",
        capability: Capability::Admin,
        handler: crate::project_fetch_rpc::handle,
    },
    // weftos.cog.v1. Exact name: a `cog.` prefix would cover methods this
    // table does not classify. Read: a start check does not spend budget.
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.check_run",
        capability: Capability::Read,
        handler: crate::cog_check_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.import",
        capability: Capability::Admin,
        handler: crate::cog_licence_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.status",
        capability: Capability::Read,
        handler: crate::cog_licence_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.claims",
        capability: Capability::Read,
        handler: crate::cog_licence_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.revoked",
        capability: Capability::Read,
        handler: crate::cog_licence_rpc::handle,
    },
    // mesh-local/1 (package H): authenticated by the spawn nonce and the
    // project key's proof of possession, not by a token; `Read` is the floor.
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.challenge",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.register",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.heartbeat",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.unregister",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
];
#[cfg(test)]
const ROUTES: &[ExtRoute] = &[
    #[cfg(all(unix, feature = "exochain"))]
    ExtRoute {
        prefix: "instance.nested.",
        capability: Capability::Admin,
        handler: crate::nested_rpc::handle,
    },
    ExtRoute {
        prefix: "kernel.handshake",
        capability: Capability::Read,
        handler: crate::handshake_rpc::handle,
    },
    ExtRoute {
        prefix: "project.list",
        capability: Capability::Read,
        handler: crate::project_rpc::handle_list,
    },
    ExtRoute {
        prefix: "project.show",
        capability: Capability::Read,
        handler: crate::project_rpc::handle_show,
    },
    ExtRoute {
        prefix: "project.register",
        capability: Capability::Admin,
        handler: crate::project_rpc::handle_register,
    },
    ExtRoute {
        prefix: "project.nested.register",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.nested.start",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.nested.stop",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.cert.show",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.cert.challenge",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.identity.repair",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.rekey",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    ExtRoute {
        prefix: "project.revoke",
        capability: Capability::Admin,
        handler: crate::project_cert_rpc::handle,
    },
    // Read: authenticated by the project key through the statement
    // signature, not by a token (a project kernel holds none).
    ExtRoute {
        prefix: "project.anchor.submit",
        capability: Capability::Read,
        handler: crate::anchor_rpc::handle,
    },
    ExtRoute {
        prefix: "project.anchor.restore",
        capability: Capability::Admin,
        handler: crate::anchor_rpc::handle_restore,
    },
    ExtRoute {
        prefix: "project.anchor.reset",
        capability: Capability::Admin,
        handler: crate::anchor_rpc::handle_reset,
    },
    // Per-project child kernels (ADR-103 A6, package G). Lifecycle is Admin;
    // `project.token.refresh` is Write so a project token can renew itself.
    ExtRoute {
        prefix: "project.start",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.ensure_running",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.stop",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.stop_all",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.restart",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.status",
        capability: Capability::Admin,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "project.token.refresh",
        capability: Capability::Write,
        handler: crate::project_lifecycle_rpc::handle,
    },
    ExtRoute {
        prefix: "auth.token.issue",
        capability: Capability::Admin,
        handler: crate::token_rpc::handle,
    },
    ExtRoute {
        prefix: "auth.token.revoke",
        capability: Capability::Admin,
        handler: crate::token_rpc::handle,
    },
    ExtRoute {
        prefix: "auth.token.list",
        capability: Capability::Admin,
        handler: crate::token_rpc::handle,
    },
    // Read: the gateway and any anonymous caller may ask "is this secret
    // valid?"; a 256-bit secret cannot be searched for.
    ExtRoute {
        prefix: "auth.token.validate",
        capability: Capability::Read,
        handler: crate::token_rpc::handle,
    },
    // Fleet manager P1: the snapshot is read-only and contacts no peer; a
    // location label is an operator decision recorded on the chain.
    ExtRoute {
        prefix: "fleet.snapshot",
        capability: Capability::Read,
        handler: crate::fleet_rpc::handle,
    },
    ExtRoute {
        prefix: "fleet.location.set",
        capability: Capability::Admin,
        handler: crate::fleet_rpc::handle,
    },
    // ADR-121 version index: read-only queries over the local version files.
    ExtRoute {
        prefix: "versions.list",
        capability: Capability::Read,
        handler: crate::versions_rpc::handle,
    },
    ExtRoute {
        prefix: "versions.tree",
        capability: Capability::Read,
        handler: crate::versions_rpc::handle,
    },
    ExtRoute {
        prefix: "versions.deps",
        capability: Capability::Read,
        handler: crate::versions_rpc::handle,
    },
    // Dashboard reporter: status is read-only; rotating the node token is Admin
    // (with `node`, it is sent to that peer over the signed mesh wire).
    ExtRoute {
        prefix: "dashboard.status",
        capability: Capability::Read,
        handler: crate::dashboard_rpc::handle,
    },
    ExtRoute {
        prefix: "dashboard.actions",
        capability: Capability::Read,
        handler: crate::dashboard_rpc::handle,
    },
    ExtRoute {
        prefix: "dashboard.token.rotate",
        capability: Capability::Admin,
        handler: crate::dashboard_rpc::handle,
    },
    // ADR-116: the tailnet router. Both are local reads of the owner's own
    // compose/ports.yaml files (the poller re-reads them anyway).
    ExtRoute {
        prefix: "route.list",
        capability: Capability::Read,
        handler: crate::router_rpc::handle,
    },
    ExtRoute {
        prefix: "route.reload",
        capability: Capability::Read,
        handler: crate::router_rpc::handle,
    },
    // ADR-108 P2b: pending pair requests on this node (request and cancel
    // are trust acts; list shows ids and node ids only).
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "mesh.pair.request",
        capability: Capability::Admin,
        handler: crate::mesh_pair_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "mesh.pair.list",
        capability: Capability::Read,
        handler: crate::mesh_pair_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "mesh.pair.cancel",
        capability: Capability::Admin,
        handler: crate::mesh_pair_rpc::handle,
    },
    // One prefix for shared.embed / shared.llm.chat / shared.llm.models: the
    // handler resolves the project itself (token scope or Admin) and refuses
    // anonymous callers; `Write` is the floor.
    // ADR-117 D1/D2: an app drives a leaf display through the daemon. Exact
    // names, not a prefix: each verb is classified in `scope_gate`.
    ExtRoute {
        prefix: "display.open",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.ops",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.poll",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.close",
        capability: Capability::Write,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.revoke",
        capability: Capability::Admin,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "display.status",
        capability: Capability::Read,
        handler: crate::display_rpc::handle,
    },
    ExtRoute {
        prefix: "shared.",
        capability: Capability::Write,
        handler: crate::shared_rpc::handle,
    },
    // Governance overlay (ADR-103 D8): push runs on the user daemon, update
    // and reload on a project kernel; all Admin.
    ExtRoute {
        prefix: "governance.parent.push",
        capability: Capability::Admin,
        handler: crate::governance_push::handle_push,
    },
    ExtRoute {
        prefix: "governance.parent.update",
        capability: Capability::Admin,
        handler: crate::governance_push::handle_update,
    },
    ExtRoute {
        prefix: "governance.reload",
        capability: Capability::Admin,
        handler: crate::governance_push::handle_reload,
    },
    // ADR-106 phase 3: Admin (a checkout spends licence budget); routed here
    // so the relay rate-limits and gates the caller's own principal.
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "workload.cog.checkout",
        capability: Capability::Admin,
        handler: crate::licence_checkout_rpc::handle_ext,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "workload.cog.checkout.release",
        capability: Capability::Admin,
        handler: crate::licence_checkout_rpc::handle_ext,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "workload.cog.checkout.renew",
        capability: Capability::Admin,
        handler: crate::licence_checkout_rpc::handle_ext,
    },
    // ADR-108 P3b: `git-remote-weftos` asks this daemon to fetch from a
    // project's primary over the signed mesh wire. Contacting a peer is Admin.
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "project.fetch",
        capability: Capability::Admin,
        handler: crate::project_fetch_rpc::handle,
    },
    // weftos.cog.v1. Exact name: a `cog.` prefix would cover methods this
    // table does not classify. Read: a start check does not spend budget.
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.check_run",
        capability: Capability::Read,
        handler: crate::cog_check_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.import",
        capability: Capability::Admin,
        handler: crate::cog_licence_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.status",
        capability: Capability::Read,
        handler: crate::cog_licence_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.claims",
        capability: Capability::Read,
        handler: crate::cog_licence_rpc::handle,
    },
    #[cfg(all(feature = "placement", unix))]
    ExtRoute {
        prefix: "cog.licence.revoked",
        capability: Capability::Read,
        handler: crate::cog_licence_rpc::handle,
    },
    // mesh-local/1 (package H): authenticated by the spawn nonce and the
    // project key's proof of possession, not by a token; `Read` is the floor.
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.challenge",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.register",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.heartbeat",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
    #[cfg(unix)]
    ExtRoute {
        prefix: "mesh.unregister",
        capability: Capability::Read,
        handler: crate::mesh_local_rpc::handle,
    },
    ExtRoute {
        prefix: "rpc_ext.test.",
        capability: Capability::Write,
        handler: test_probe,
    },
];

/// Prefix or exact name and required capability of each built-in route
/// (scope-gate population test).
#[cfg(test)]
pub(crate) fn builtin_route_names() -> Vec<(&'static str, Capability)> {
    ROUTES.iter().map(|r| (r.prefix, r.capability)).collect()
}

/// Registered gates, run in order; the first denial wins.
///
/// The D12 scope gates (package G): the voice deny-list, then the
/// outside-project policy.
#[cfg(not(test))]
const GATES: &[GateFn] = &[
    crate::scope_gate::voice_gate,
    crate::licence_role_gate::licence_role_gate,
    crate::scope_gate::scope_gate,
];
#[cfg(test)]
const GATES: &[GateFn] = &[
    test_deny_gate,
    crate::scope_gate::voice_gate,
    crate::licence_role_gate::licence_role_gate,
    crate::scope_gate::scope_gate,
];

/// Unit-test-only route: proves extension dispatch on the real wire path.
#[cfg(test)]
fn test_probe(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        Response::success(serde_json::json!({
            "method": call.method,
            "auth": call.ctx.auth,
            "project": call.ctx.project.as_ref().map(ClaimedProject::as_str),
        }))
    })
}

/// Unit-test-only gate: denies `rpc_ext.gated.*` on the real wire path.
#[cfg(test)]
fn test_deny_gate<'a>(req: &'a GateRequest<'a>) -> GateFuture<'a> {
    Box::pin(async move {
        if req.method.starts_with("rpc_ext.gated.") {
            Err(Denial::new("scope_denied", "test gate denies rpc_ext.gated.*"))
        } else {
            Ok(())
        }
    })
}

/// A set of routes and gates. Production uses [`ExtRegistry::builtin`]
/// (zero-cost: borrows the const tables); tests compose their own.
#[derive(Clone)]
pub struct ExtRegistry {
    routes: Cow<'static, [ExtRoute]>,
    gates: Cow<'static, [GateFn]>,
}

impl Default for ExtRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtRegistry {
    pub fn new() -> Self {
        Self {
            routes: Cow::Owned(Vec::new()),
            gates: Cow::Owned(Vec::new()),
        }
    }

    /// The registry built from the `ROUTES` / `GATES` tables.
    pub fn builtin() -> Self {
        Self {
            routes: Cow::Borrowed(ROUTES),
            gates: Cow::Borrowed(GATES),
        }
    }

    pub fn route(
        mut self,
        prefix: &'static str,
        capability: Capability,
        handler: ExtHandler,
    ) -> Self {
        self.routes.to_mut().push(ExtRoute {
            prefix,
            capability,
            handler,
        });
        self
    }

    pub fn gate(mut self, gate: GateFn) -> Self {
        self.gates.to_mut().push(gate);
        self
    }

    fn find(&self, method: &str) -> Option<&ExtRoute> {
        self.routes.iter().find(|r| r.matches(method))
    }

    /// Capability `method` requires: the route's explicit one, else the
    /// legacy table.
    fn required(&self, method: &str) -> Capability {
        self.find(method)
            .map(|r| r.capability)
            .unwrap_or_else(|| required_capability(method))
    }
}

/// Authorize a request: capability check, then gates. `Err` carries the
/// refusal response (without request id; the caller attaches it).
///
/// Every entry path calls this once per request before any dispatch.
pub async fn authorize_with(
    registry: &ExtRegistry,
    caller: &CallerCtx,
    caps: &CallerCapabilities,
    method: &str,
    params: &Value,
    kernel: &KernelRef,
) -> Result<(), Response> {
    let required = registry.required(method);
    if !caps.allows(required) {
        tracing::warn!(
            method = %method,
            required = ?required,
            "rpc capability check failed; rejecting"
        );
        return Err(Response::error(format!(
            "permission denied: method '{method}' requires capability {required:?}"
        )));
    }
    let req = GateRequest {
        principal: caller.principal,
        method,
        params,
        auth: caller.auth.as_deref(),
        project: caller.project.as_ref().map(ClaimedProject::as_str),
        verified_project: caller.verified_project.as_ref(),
        caps,
        kernel,
    };
    for gate in registry.gates.iter() {
        if let Err(d) = gate(&req).await {
            return Err(Response::error_with_kind(d.kind, d.message));
        }
    }
    Ok(())
}

/// [`authorize_with`] over the built-in registry.
pub async fn authorize(
    caller: &CallerCtx,
    caps: &CallerCapabilities,
    method: &str,
    params: &Value,
    kernel: &KernelRef,
) -> Result<(), Response> {
    authorize_with(&ExtRegistry::builtin(), caller, caps, method, params, kernel).await
}

/// Run `registry`'s handler for `method`, or `None` if no route matches.
/// Call only after [`authorize_with`] succeeded.
pub async fn dispatch_ext_with(
    registry: &ExtRegistry,
    caller: &CallerCtx,
    caps: &CallerCapabilities,
    method: &str,
    params: &Value,
    kernel: &KernelRef,
) -> Option<Response> {
    let route = registry.find(method)?;
    Some(
        (route.handler)(ExtCall {
            method: method.to_owned(),
            params: params.clone(),
            ctx: ExtCtx {
                kernel: Arc::clone(kernel),
                auth: caller.auth.clone(),
                project: caller.project.clone(),
                verified_project: caller.verified_project.clone(),
                caps: caps.clone(),
            },
        })
        .await,
    )
}

/// [`dispatch_ext_with`] over the built-in registry; `None` means the
/// legacy `match` should handle `method`.
pub async fn dispatch_ext(
    caller: &CallerCtx,
    caps: &CallerCapabilities,
    method: &str,
    params: &Value,
    kernel: &KernelRef,
) -> Option<Response> {
    dispatch_ext_with(&ExtRegistry::builtin(), caller, caps, method, params, kernel).await
}

#[cfg(test)]
#[path = "rpc_ext_tests.rs"]
mod tests;
