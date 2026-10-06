//! Per-method capability gating for daemon RPC dispatch (WEFT-479).
//!
//! Today the daemon's UDS listener accepts every JSON-RPC verb from
//! every caller. There is no notion of "this caller may call
//! `kernel.shutdown` but not `agent.spawn`". This module is the
//! minimum honest gate: each method declares a required
//! [`Capability`], and an effective capability set is computed for
//! the caller (anonymous by default, escalated when a Bearer header
//! is presented and matches a token issued by the kernel's
//! [`AuthService`](clawft_kernel::AuthService)).
//!
//! # Capability classes
//!
//! - [`Capability::Read`] — read-only verbs that don't mutate state
//!   (`kernel.status`, `kernel.ps`, `agent.list`, ...). Anonymous
//!   callers always have this.
//! - [`Capability::Chat`] — conversational verbs that the LLM-side
//!   integration needs (`agent.chat`, `agent.chat.cancel`). Granted
//!   to anonymous callers by default; an operator can tighten this
//!   later by removing `Chat` from the anonymous baseline.
//! - [`Capability::Write`] — mutating verbs that change agent or
//!   substrate state (`agent.spawn`, `agent.stop`, `agent.send`,
//!   `memory.delete`, `substrate.publish`, ...). Requires
//!   authentication.
//! - [`Capability::Admin`] — destructive verbs that affect the
//!   daemon process itself (`kernel.shutdown`, `kernel.kill-process`,
//!   `kernel.restart-service`). Requires authentication AND the
//!   token's scope must include `admin`.
//!
//! # Posture
//!
//! Default-permissive on read; default-deny on admin. The
//! anonymous baseline includes `Read` and `Chat` (back-compat
//! posture for existing UDS callers). `Write` and `Admin` require an
//! authenticated token. The wire format adds an optional `auth`
//! field to the JSON-RPC request envelope; absent or empty `auth`
//! defaults to anonymous.

use std::collections::HashSet;

/// A discrete privilege the daemon RPC dispatcher checks before
/// executing a method handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Read-only inspection (no state change).
    Read,
    /// Conversational verbs (agent.chat, etc.).
    Chat,
    /// Mutating writes to agents / substrate / memory.
    Write,
    /// Destructive admin verbs (shutdown, kill, restart).
    Admin,
}

/// True when `token` is nothing but the reserved literal scope strings
/// (`admin`, `write`, `chat`, `read`, comma-separated): a self-asserted
/// scope, not a credential. Only the local unix-socket owner may use it.
pub fn is_literal_scope(token: &str) -> bool {
    let known = ["admin", "write", "chat", "read"];
    let t = token.trim();
    !t.is_empty() && t.split(',').map(str::trim).all(|p| known.contains(&p))
}

/// Look up the [`Capability`] required by a given JSON-RPC method.
///
/// Methods not in the table default to [`Capability::Read`]. This
/// keeps unknown verbs callable by anonymous clients (so a typo
/// surfaces as "unknown method" from the dispatcher rather than
/// silent permission-denied), while ensuring the four classes of
/// dangerous verbs we DO know about are gated correctly. As new
/// verbs land, the author should add them here.
pub fn required_capability(method: &str) -> Capability {
    match method {
        // ── Admin: destructive verbs that affect the daemon itself ─
        "kernel.shutdown" => Capability::Admin,
        "kernel.kill-process" => Capability::Admin,
        "kernel.restart-service" => Capability::Admin,
        "cluster.join" => Capability::Admin,
        "cluster.leave" => Capability::Admin,
        "chain.checkpoint" => Capability::Admin,
        // ADR-099: revocation and device-to-node binding are trust-root
        // changes, so Admin rather than Write.
        "workload.revoke" => Capability::Admin,
        // mesh-placement-19: who may use whose model server is a governed
        // decision, the same standing as a revocation.
        "infer.expose" | "infer.allow" | "infer.start" | "infer.stop" => Capability::Admin,
        // ADR-120: switching an exposure and reading who called it are local
        // operator acts; the log names callers, so it is not anonymous Read.
        "expose.set" | "expose.off_all" | "expose.log" => Capability::Admin,
        "workload.node.bind" => Capability::Admin,
        // ADR-106: withdrawing a Seed binding is the same trust change.
        "workload.node.unbind" => Capability::Admin,
        "workload.node.reset-floor" => Capability::Admin,
        // Fleet manager: a location label is an operator decision on the chain.
        "fleet.location.set" => Capability::Admin,
        // Rotating the node's dashboard credential (locally or on a peer).
        "dashboard.token.rotate" => Capability::Admin,
        // ADR-108 P3b: fetching a project from its primary contacts a peer.
        "project.fetch" => Capability::Admin,
        // ADR-108 P2b: asking to pair (or withdrawing the ask) is a trust
        // request this node makes in its own name.
        "mesh.pair.request" | "mesh.pair.cancel" => Capability::Admin,
        // ADR-106 phase 3: an operator hash approval is a trust change, and a
        // checkout spends the Seed's licence and transfer budget.
        "workload.cog.checkout.approve" => Capability::Admin,
        "workload.cog.checkout" => Capability::Admin,
        // ADR-106: ending a checkout and renewing on demand spend the Seed's budget.
        "workload.cog.checkout.release" => Capability::Admin,
        "workload.cog.checkout.renew" => Capability::Admin,

        // ── Write: state-mutating verbs ─────────────────────────────
        "agent.register" => Capability::Write,
        "agent.spawn" => Capability::Write,
        "agent.stop" => Capability::Write,
        "agent.restart" => Capability::Write,
        "agent.send" => Capability::Write,
        "node.register" => Capability::Write,
        "memory.delete" => Capability::Write,
        "substrate.publish" => Capability::Write,
        "substrate.canonical_publish_payload" => Capability::Write,
        "substrate.notify" => Capability::Write,
        "control.set_enabled" => Capability::Write,
        "terminal.spawn" => Capability::Write,
        "terminal.write" => Capability::Write,
        "terminal.resize" => Capability::Write,
        "terminal.close" => Capability::Write,
        // WEFT-654 review gate: accept/discard mutate the loop's held
        // proposal (promote or prune + witness) — same class as the other
        // state-mutating agent verbs above.
        "agent.proposal.accept" => Capability::Write,
        "agent.proposal.discard" => Capability::Write,
        // WEFT-324: public witness append (soul promote + agent journal).
        // Write — not Admin — so a write-scoped token can promote without
        // holding full daemon admin. Checkpoint remains Admin.
        "chain.append" => Capability::Write,
        // WEFT-150: topic publish mutates the live IPC / mesh fan-out
        // (including `weaver leaf push` → `mesh.leaf.<pk>.push`). Anonymous
        // must not publish; DaemonClient on the local UDS path auto-attaches
        // `admin` so operator CLI continues to work.
        "ipc.publish" => Capability::Write,
        // WEFT-494 / ADR-070: live MCP registry mutations.
        "mcp.add" => Capability::Write,
        "mcp.remove" => Capability::Write,
        "mcp.reload" => Capability::Write,
        // mesh-placement-06: `weaver app` lifecycle mutates the app catalog
        // and spawns/stops agents.
        "app.install" | "app.start" | "app.stop" | "app.remove" => Capability::Write,
        // ADR-099 section 4 workload lifecycle actions.
        "workload.install"
        | "workload.place"
        | "workload.load"
        | "workload.start"
        | "workload.stop"
        | "workload.unload"
        | "workload.migrate"
        // mesh-placement-12: these sign/contact peers or expose cog output.
        | "workload.explain"
        | "workload.status"
        | "workload.logs"
        // Cron mutations change what the kernel will run on its own.
        | "cron.add"
        | "cron.remove"
        | "cron.enable"
        | "cron.disable" => Capability::Write,

        // ── Chat: LLM-conversational verbs ──────────────────────────
        "agent.chat" => Capability::Chat,
        // WEFT-253: progressive companion to agent.chat (same Chat cap).
        "agent.chat_stream" => Capability::Chat,
        "agent.chat.cancel" => Capability::Chat,
        "agent.chat.end" => Capability::Chat,
        // WEFT-331: human decision for interactive gate Defer.
        "agent.chat.defer_decide" => Capability::Chat,
        "agent.turn.record" => Capability::Chat,
        "llm.prompt" => Capability::Chat,

        // ── Read: everything else explicitly classified ─────────────
        "kernel.status"
        | "kernel.ps"
        | "kernel.services"
        | "kernel.logs"
        // WEFT-434: live log tail (same Read capability as kernel.logs).
        | "kernel.logs_stream"
        | "cluster.status"
        | "cluster.nodes"
        // mesh-placement-03: signed node facts (read-only; `refresh`
        // re-probes the local node but changes no durable state).
        | "cluster.facts"
        | "cluster.health"
        | "cluster.shards"
        | "chain.status"
        // ADR-103 P2 D: streaming chain tail; `project/<id>` adds its own
        // verified-project check in the handler.
        | "chain.subscribe"
        | "chain.local"
        | "chain.verify"
        // WEFT-125: active vector backend introspection (read-only).
        | "ecc.vector-config"
        | "agent.inspect"
        | "agent.list"
        | "agent.proposal.list"
        | "control.list"
        | "node.identity"
        | "substrate.read"
        | "substrate.list"
        | "substrate.subscribe"
        | "ipc.subscribe_stream"
        // WEFT-256: model/provider enumeration (read-only; no state change).
        | "llm.models"
        // WEFT-494: live MCP registry inspection (alias tools.mcp = mcp.list).
        | "mcp.list"
        | "tools.mcp"
        // mesh-placement-06: catalog inspection.
        | "app.list"
        | "app.inspect"
        | "workload.list"
        | "workload.inspect"
        // ADR-106: binding status (mesh id, held binding, orphaned or not);
        // public facts, and what `weaver doctor` reads.
        | "workload.node.binding"
        // ADR-106 phase 3: grants, approvals and the run gate per artifact.
        | "workload.cog.checkout.status"
        | "workload.cog.checkout.list"
        // weftos.cog.v1: Cog Host's start check. Read, same standing as checkout status.
        | "cog.check_run"
        | "infer.status"
        | "expose.list"
        // Fleet manager: one read-only document composed from sources the
        // daemon already holds; contacts no peer and no device.
        | "fleet.snapshot"
        // ADR-121: read-only queries over the local version index.
        | "versions.list"
        | "versions.tree"
        | "versions.deps"
        // Reporter state without token material; a `node` that is another
        // peer needs Admin inside the handler (it contacts that peer).
        | "dashboard.status"
        // Recent dashboard actions and their outcomes (no payloads); local only.
        | "dashboard.actions"
        // Pending pair requests (ids, node ids, project ULIDs; no key material).
        | "mesh.pair.list" => Capability::Read,

        // An unclassified `dashboard.*` verb is a mutation, never anonymous Read.
        m if m.starts_with("dashboard.") && m != "dashboard.status" && m != "dashboard.actions" => Capability::Admin,

        // An unclassified `versions.*` verb is refused as a mutation, never anonymous Read.
        m if m.starts_with("versions.") => Capability::Admin,

        // An unclassified `fleet.*` verb is a mutation, never anonymous Read.
        m if m.starts_with("fleet.") => Capability::Admin,

        // An unclassified `infer.*` verb is a mutation, never anonymous Read.
        m if m.starts_with("infer.") => Capability::Admin,

        // ADR-120: likewise for an unclassified `expose.*` verb.
        m if m.starts_with("expose.") => Capability::Admin,

        // ADR-099 default-deny posture: an unclassified `workload.*` verb
        // is treated as a mutation, never as anonymous-callable Read.
        m if m.starts_with("workload.") => Capability::Write,

        // Default for anything we haven't explicitly classified.
        // Read is the safest baseline — the verb still goes through
        // its own per-handler validation. New verbs should be added
        // to this table as they're written.
        _ => Capability::Read,
    }
}

/// The effective capability set for a single RPC caller.
///
/// Built per-request from the optional `auth` field on the JSON-RPC
/// envelope:
///
/// - No `auth` (or empty string) → [`Self::anonymous`]: `{Read, Chat}`.
/// - `auth` matches a kernel-issued token → token scopes are mapped
///   to the corresponding capabilities (e.g. scope `"admin"` →
///   `Capability::Admin`).
/// - `auth` present but does NOT match any token → [`Self::denied`]:
///   empty set (every gated verb fails). Anonymous would have been
///   the safer default but a token that LOOKED valid getting silently
///   downgraded would mask a misconfiguration; deny instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerCapabilities {
    granted: HashSet<Capability>,
}

impl CallerCapabilities {
    /// Anonymous baseline: `{Read, Chat}`.
    pub fn anonymous() -> Self {
        let mut granted = HashSet::new();
        granted.insert(Capability::Read);
        granted.insert(Capability::Chat);
        Self { granted }
    }

    /// Empty set — every gated verb fails. Use when an `auth` token
    /// was presented but did not validate.
    pub fn denied() -> Self {
        Self {
            granted: HashSet::new(),
        }
    }

    /// Construct from an explicit set of scope strings (typically
    /// from `AuthToken::scopes`). Recognised scopes:
    ///
    /// - `"read"` → [`Capability::Read`]
    /// - `"chat"` → [`Capability::Chat`]
    /// - `"write"` → [`Capability::Write`]
    /// - `"admin"` → [`Capability::Admin`] (also implies the others)
    ///
    /// Unknown scopes are ignored. An authenticated caller always
    /// gets at least `Read`.
    pub fn from_scopes<I, S>(scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut granted = HashSet::new();
        granted.insert(Capability::Read);
        for scope in scopes {
            match scope.as_ref() {
                "read" => {
                    granted.insert(Capability::Read);
                }
                "chat" => {
                    granted.insert(Capability::Chat);
                }
                "write" => {
                    granted.insert(Capability::Write);
                }
                "admin" => {
                    granted.insert(Capability::Read);
                    granted.insert(Capability::Chat);
                    granted.insert(Capability::Write);
                    granted.insert(Capability::Admin);
                }
                _ => {}
            }
        }
        Self { granted }
    }

    /// True when this caller may invoke a method requiring `cap`.
    pub fn allows(&self, cap: Capability) -> bool {
        self.granted.contains(&cap)
    }

    /// True when this caller may invoke `method`.
    pub fn allows_method(&self, method: &str) -> bool {
        self.allows(required_capability(method))
    }
}

impl Default for CallerCapabilities {
    fn default() -> Self {
        Self::anonymous()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_can_read_and_chat() {
        let caps = CallerCapabilities::anonymous();
        assert!(caps.allows(Capability::Read));
        assert!(caps.allows(Capability::Chat));
        assert!(!caps.allows(Capability::Write));
        assert!(!caps.allows(Capability::Admin));
    }

    #[test]
    fn anonymous_method_gating() {
        let caps = CallerCapabilities::anonymous();
        assert!(caps.allows_method("kernel.status"));
        assert!(caps.allows_method("agent.list"));
        assert!(caps.allows_method("agent.chat"));
        // WEFT-253: progressive chat shares Chat capability.
        assert!(caps.allows_method("agent.chat_stream"));
        assert_eq!(required_capability("agent.chat_stream"), Capability::Chat);
        // WEFT-256: model enumeration is Read (anonymous-safe).
        assert!(caps.allows_method("llm.models"));
        assert_eq!(required_capability("llm.models"), Capability::Read);
        // WEFT-125: vector backend introspection is Read (anonymous-safe).
        assert!(caps.allows_method("ecc.vector-config"));
        assert_eq!(required_capability("ecc.vector-config"), Capability::Read);
        // WEFT-331: interactive defer decision (panel allow/deny/cancel).
        assert!(caps.allows_method("agent.chat.defer_decide"));
        assert_eq!(
            required_capability("agent.chat.defer_decide"),
            Capability::Chat
        );
        // gated:
        assert!(!caps.allows_method("agent.spawn"));
        assert!(!caps.allows_method("memory.delete"));
        assert!(!caps.allows_method("kernel.shutdown"));
        assert!(!caps.allows_method("kernel.kill-process"));
    }

    #[test]
    fn denied_set_blocks_everything_gated() {
        let caps = CallerCapabilities::denied();
        // Read still works because Read is in the empty set's view —
        // wait, no: denied is empty, so even Read fails. This is the
        // intended behaviour: a presented-but-invalid token is a
        // misconfiguration that should NOT silently fall back to
        // anonymous.
        assert!(!caps.allows(Capability::Read));
        assert!(!caps.allows_method("kernel.status"));
        assert!(!caps.allows_method("agent.spawn"));
        assert!(!caps.allows_method("kernel.shutdown"));
    }

    #[test]
    fn admin_scope_implies_all() {
        let caps = CallerCapabilities::from_scopes(["admin"]);
        assert!(caps.allows(Capability::Read));
        assert!(caps.allows(Capability::Chat));
        assert!(caps.allows(Capability::Write));
        assert!(caps.allows(Capability::Admin));
        assert!(caps.allows_method("kernel.shutdown"));
        assert!(caps.allows_method("agent.spawn"));
    }

    #[test]
    fn write_scope_does_not_imply_admin() {
        let caps = CallerCapabilities::from_scopes(["write"]);
        assert!(caps.allows_method("agent.spawn"));
        assert!(caps.allows_method("memory.delete"));
        assert!(!caps.allows_method("kernel.shutdown"));
        assert!(!caps.allows_method("kernel.kill-process"));
    }

    #[test]
    fn unknown_scopes_ignored() {
        let caps = CallerCapabilities::from_scopes(["bogus", "read"]);
        assert!(caps.allows(Capability::Read));
        assert!(!caps.allows(Capability::Write));
    }

    #[test]
    fn unknown_methods_default_to_read() {
        assert_eq!(
            required_capability("zzz.never_heard_of_this"),
            Capability::Read
        );
    }

    #[test]
    fn admin_methods_classified_correctly() {
        for m in [
            "kernel.shutdown",
            "kernel.kill-process",
            "kernel.restart-service",
            "cluster.join",
            "cluster.leave",
            "chain.checkpoint",
        ] {
            assert_eq!(
                required_capability(m),
                Capability::Admin,
                "method {m} should be Admin",
            );
        }
    }

    #[test]
    fn chain_append_requires_write() {
        assert_eq!(required_capability("chain.append"), Capability::Write);
        let anon = CallerCapabilities::anonymous();
        assert!(
            !anon.allows_method("chain.append"),
            "anonymous must not append to the witness chain"
        );
        let write = CallerCapabilities::from_scopes(["write"]);
        assert!(write.allows_method("chain.append"));
        let admin = CallerCapabilities::from_scopes(["admin"]);
        assert!(admin.allows_method("chain.append"));
    }

    #[test]
    fn write_methods_classified_correctly() {
        for m in [
            "agent.register",
            "agent.spawn",
            "chain.append",
            "ipc.publish",
            "agent.stop",
            "agent.send",
            "memory.delete",
            "substrate.publish",
            "terminal.spawn",
        ] {
            assert_eq!(
                required_capability(m),
                Capability::Write,
                "method {m} should be Write",
            );
        }
    }

    #[test]
    fn ipc_publish_requires_write() {
        // WEFT-150: leaf-push and generic topic publish are Write-gated.
        assert_eq!(required_capability("ipc.publish"), Capability::Write);
        let anon = CallerCapabilities::anonymous();
        assert!(
            !anon.allows_method("ipc.publish"),
            "anonymous must not publish to IPC topics (incl. mesh.leaf.*.push)"
        );
        let write = CallerCapabilities::from_scopes(["write"]);
        assert!(write.allows_method("ipc.publish"));
        let admin = CallerCapabilities::from_scopes(["admin"]);
        assert!(admin.allows_method("ipc.publish"));
    }

    #[test]
    fn inference_verbs_are_classified() {
        // mesh-placement-19: status reads; who may use whose server is Admin,
        // and an unclassified `infer.*` verb is never anonymous Read.
        let anon = CallerCapabilities::anonymous();
        let write = CallerCapabilities::from_scopes(["write"]);
        let admin = CallerCapabilities::from_scopes(["admin"]);
        assert!(anon.allows_method("infer.status"));
        for m in ["infer.expose", "infer.allow", "infer.somethingnew"] {
            assert!(!anon.allows_method(m), "{m}");
            assert!(!write.allows_method(m), "{m}");
            assert!(admin.allows_method(m), "{m}");
        }
    }

    #[test]
    fn exposure_verbs_are_classified() {
        let anon = CallerCapabilities::anonymous();
        let write = CallerCapabilities::from_scopes(["write"]);
        let admin = CallerCapabilities::from_scopes(["admin"]);
        assert!(anon.allows_method("expose.list"));
        for m in ["expose.set", "expose.off_all", "expose.log", "expose.somethingnew"] {
            assert!(!anon.allows_method(m), "{m}");
            assert!(!write.allows_method(m), "{m}");
            assert!(admin.allows_method(m), "{m}");
        }
    }

    #[test]
    fn app_and_workload_verbs_classified() {
        // mesh-placement-06.
        let anon = CallerCapabilities::anonymous();
        let write = CallerCapabilities::from_scopes(["write"]);
        for m in [
            "app.list",
            "app.inspect",
            "workload.list",
            "workload.inspect",
            "workload.node.binding",
            "workload.cog.checkout.status",
            "workload.cog.checkout.list",
        ] {
            assert_eq!(required_capability(m), Capability::Read, "{m}");
            assert!(anon.allows_method(m), "{m}");
        }
        for m in [
            "app.install",
            "app.start",
            "app.stop",
            "app.remove",
            "workload.install",
            "workload.place",
            "workload.explain",
            "workload.status",
            "workload.logs",
            "workload.load",
            "workload.start",
            "workload.stop",
            "workload.unload",
            "workload.migrate",
            "workload.some_future_verb",
        ] {
            assert_eq!(required_capability(m), Capability::Write, "{m}");
            assert!(!anon.allows_method(m), "anonymous must not call {m}");
            assert!(write.allows_method(m), "{m}");
        }
        for m in [
            "workload.revoke",
            "workload.node.bind",
            "workload.node.unbind",
            "workload.node.reset-floor",
            "workload.cog.checkout.approve",
            "workload.cog.checkout",
            "workload.cog.checkout.release",
            "workload.cog.checkout.renew",
        ] {
            assert_eq!(required_capability(m), Capability::Admin, "{m}");
            assert!(!write.allows_method(m), "{m}");
        }
    }

    #[test]
    fn mcp_registry_verbs_classified() {
        // WEFT-494 / ADR-070.
        assert_eq!(required_capability("mcp.list"), Capability::Read);
        assert_eq!(required_capability("tools.mcp"), Capability::Read);
        for m in ["mcp.add", "mcp.remove", "mcp.reload"] {
            assert_eq!(
                required_capability(m),
                Capability::Write,
                "method {m} should be Write"
            );
        }
        let anon = CallerCapabilities::anonymous();
        assert!(anon.allows_method("mcp.list"));
        assert!(!anon.allows_method("mcp.add"));
        assert!(!anon.allows_method("mcp.remove"));
        assert!(!anon.allows_method("mcp.reload"));
    }
}
