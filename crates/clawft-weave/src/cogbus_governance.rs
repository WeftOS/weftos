//! The cogbus's one `GovernanceRequest` site (ADR-122 section 5, B3).
//!
//! Attribution: the request is built with `GovernanceRequest::new`, which
//! calls `attributed()`, so its project and instance principal come from
//! the kernel's own attestation (ADR-103 A6), never from the cog. The agent
//! id is the cog instance the broker authenticated by its token. The context
//! carries only the topic (as `tool`), cog id and schema, all from that
//! instance's placement registration and the request the broker already
//! checked against it; no project is read from anything the cog sends.
//! Cross-project access is the broker's own rule plus `cogbus-policy.json`
//! grants, not a governance context key.

use clawft_kernel::governance::{GovernanceDecision, GovernanceEngine, GovernanceRequest};

/// What a cogbus decision is about.
#[derive(Debug, Clone, Copy)]
pub struct BusAsk<'a> {
    /// Authenticated instance id (the agent).
    pub instance: &'a str,
    /// `cogbus.declare` / `.publish` / `.subscribe`.
    pub action: &'a str,
    /// Topic, wildcard or kind.
    pub topic: &'a str,
    /// The instance's registered cog id.
    pub cog_id: &'a str,
    /// Schema id.
    pub schema: &'a str,
}

/// Evaluate `ask` on `engine`; `Err` is the refusal reason. Not chained
/// (ADR-122: no chain event per subscription).
pub fn evaluate(engine: &GovernanceEngine, ask: BusAsk<'_>) -> Result<(), String> {
    let req = GovernanceRequest::new(ask.instance, ask.action)
        .with_context_entry("tool", ask.topic)
        .with_context_entry("cog_id", ask.cog_id)
        .with_context_entry("schema", ask.schema);
    match engine.evaluate(&req).decision {
        GovernanceDecision::Permit | GovernanceDecision::PermitWithWarning(_) => Ok(()),
        GovernanceDecision::Deny(why) => Err(why),
        // Nobody answers an approval on a socket: refused.
        GovernanceDecision::EscalateToHuman(why) => Err(format!("needs approval: {why}")),
        other => Err(format!("governance answered {other:?}")),
    }
}
