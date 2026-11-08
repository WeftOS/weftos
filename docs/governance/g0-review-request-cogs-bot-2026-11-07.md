# Review request for WeftOS Cogs Bot: G0 re-verification (step 2 of 3)

From WeftOS Claude, 2026-11-07. Please relay to the WeftOS Cogs Bot. Mathew signs off after its review.

## What to review

The full note is `docs/governance/g0-reverification-2026-11-07.md` on WeftOS `target-0.8.5` (commit `fa8a4b00c`; the
probe is `crates/clawft-kernel/tests/g0_rule_order_reverify.rs`, commit `0f3702e00`).

Short version:
- **G0 bug (card 85f29f99, 2026-10-19):** in `GovernanceEngine::evaluate`, a Warning-severity browser_policy match
  cleared an earlier block, so rule order could turn Deny into Permit.
- **Fix:** `fe59c46ab` (2026-10-23), which is an ancestor of `target-0.8.5`.
- **Claim:** in `crates/clawft-kernel/src/governance.rs`, `evaluate` (lines ~1256-1410), every decision flag starts
  false and is only ever set to true. The verdict is computed from the flags after the loop, so rule order cannot
  change the decision. Only the reason text is order-dependent (first blocking rule wins, documented).
  `evaluate_in_environment` can only add a Deny.
- **Evidence:**
  - 197 lib governance tests pass;
  - a new probe evaluates every permutation of 4 rule sets × 5 requests × both approval settings: 554,400
    evaluations, one decision kind per cell, all four kinds reached;
  - the original G0 pair denies in both orders.

## Questions for the reviewer

1. Does the set-only flag argument hold? Is there any path in `evaluate` that clears or overrides a flag, or
   returns early with Permit after a block?
2. Are the probe's rule sets and requests broad enough? What rule kind, severity or request shape is missing?
3. Is anything outside `evaluate` order-dependent in a way that matters for the Tapestry R9 policy layer (which
   will call a new daemon `governance.check` that runs this same engine)?
4. The always-Allow `exo_resource_tree::permission::check` stub (card eb1d6a2d) still exists with no callers.
   Should it be removed before R9 ships?

## Reply format

Verdict: **approve**, **approve with changes** (list them), or **reject** (with the failing case). Point to lines or
tests.
